//! `services/live.py` — the live tab's read side: burn, latency, watermarks.
//!
//! | Python | Rust |
//! |---|---|
//! | `_table_exists` | [`mart_queries::table_exists`](crate::services::mart_queries::table_exists) (law 7 — `type='table'`) |
//! | `_now_utc` | `pytime::now_micros` |
//! | `_iso_to_dt` | [`iso_to_dt`] |
//! | `max_event_id` / `max_tool_call_id` | [`max_event_id`] / [`max_tool_call_id`] |
//! | `recent_events` / `recent_tool_calls` | [`recent_events`] / [`recent_tool_calls`] |
//! | `BURN_TODAY_CACHE_TTL_SECONDS` | [`BURN_TODAY_CACHE_TTL_SECONDS`] |
//! | `_day_str` / `_burn_cutoffs` | [`day_str`] / [`burn_cutoffs`] |
//! | `_window_cost` / `_today_month_cost` | [`window_cost`] / [`today_month_cost`] |
//! | `rolling_burn` | [`rolling_burn`] |
//! | `_percentile` | [`percentile`] |
//! | `_MAX_BOUND_SESSIONS` / `_LATENCY_LEAD_SQL` | [`MAX_BOUND_SESSIONS`] / [`LATENCY_LEAD_SQL`] |
//! | `_latency_samples` | [`latency_samples`] |
//! | `tool_latency_percentiles` | [`tool_latency_percentiles`] |
//! | `snapshot` | [`snapshot`] |
//!
//! # The three things that decide whether this port is right
//!
//! **1. The local day here is arithmetic, not a timezone.** DIV-141 flagged
//! `rolling_burn` as `/etc/localtime`-dependent by analogy with DIV-093. It is
//! not. `_now_utc()` is `datetime.now(UTC)` and every boundary is
//! `now + timedelta(minutes=tz_offset)` — the caller's minutes-east-of-UTC
//! offset and nothing else. Measured: the same call under `TZ=UTC`,
//! `TZ=Asia/Tokyo` and `TZ=America/Los_Angeles` returns byte-identical
//! today/MTD/projection figures on the shared home, while `date.today()` in the
//! same three processes moves to `2026-08-01` under Tokyo. So there is no
//! process-timezone seam in this module to reproduce, and introducing one would
//! be the divergence.
//!
//! **2. `now + timedelta(minutes=tz_offset)` can raise.** `timezone_offset` is
//! NOT clamped on this endpoint (unlike `/api/stats`, which pins to
//! `[-720, 840]`), so a caller can push the local wall clock outside
//! `datetime`'s `[year 1, year 9999]` and CPython raises
//! `OverflowError: date value out of range` from `services/live.py:203` —
//! measured, a plain-text `500`, not a validation error. [`burn_cutoffs`]
//! returns `None` for exactly that case so the route can reproduce the status
//! *and* the body. Note the asymmetry the arithmetic forces: `+2147483647`
//! answers `200` (year 6109 is inside the range) while `-2147483648` is `500`
//! (year -2057 is not).
//!
//! **3. The latency SQL is a SHAPE, not a result set.** `_latency_samples`
//! runs two statements over a `messages` object that is a **UNION-ALL VIEW over
//! monthly partitions**. The floor is hoisted into a bound literal and the
//! session predicate is the list-subquery idiom, and the reason is written into
//! the query plan: with those two shapes SQLite emits one
//! `SEARCH messages_<ym> USING INDEX … (session_fk=?)` per arm and evaluates
//! the session list ONCE (`LIST SUBQUERY 1`, then `REUSE LIST SUBQUERY 1` on
//! every remaining arm); with the pre-fix `id >= (SELECT MIN(message_id) …)`
//! scalar-subquery floor it emits `SCAN messages_<ym>` on every arm and re-runs
//! the mart aggregate per arm. Measured on the shared 3.9 GB store, 16
//! partitions: 16 `SEARCH` and 0 `SCAN messages_*` for the new shape, against
//! 16 `SCAN messages_*` for the old one.
//! `the_latency_plan_searches_every_partition_and_hoists_the_session_list`
//! asserts both halves and
//! `the_pre_fix_scalar_subquery_floor_scans_every_partition` is the
//! counterfactual, because a query that merely returns the right rows is how
//! the July hangs shipped the first time.
//!
//! # Smaller things a careless port gets wrong
//!
//! * **`_percentile`'s comment disagrees with `_percentile`'s code.** The
//!   docstring says `ceil(p/100 * N) - 1`; the code is `int((p / 100.0) * N)`
//!   clamped to `[0, N-1]`. The code is the contract, and it must be evaluated
//!   in `f64`: `0.95 * 61` is `57.949999999999996`, so P95 of 61 samples is
//!   index 57, not 58.
//! * **`sorted(vals)` then `out.sort(key=lambda x: -x["samples"])`.** Python's
//!   sort is stable, so tools with equal sample counts keep the order they
//!   first appeared in the `win` row scan — a `dict` insertion order, which is
//!   why [`latency_samples`] returns a `Vec` of pairs and not a map.
//! * **`float(row[0] or 0.0)`** is `NULL → 0.0`; there is no `sum()` anywhere
//!   in this module, so law 3's compensation question never arises — every
//!   total is SQLite's `SUM`.
//! * **`if r[2]`** on the session id is truthiness: `NULL` *and* `""` are both
//!   dropped before the `IN (…)` list is built.

use rusqlite::Connection;
use serde_json::{Map, Value};
use stax_core::queries::pytime;
use stax_etl::stats::pydatetime::{self, PyDateTime};

use crate::services::mart_queries::table_exists;
use crate::services::plans::days_in_month;

/// `BURN_TODAY_CACHE_TTL_SECONDS` — how long a stream reuses today/MTD.
pub const BURN_TODAY_CACHE_TTL_SECONDS: f64 = 30.0;

/// `_MAX_BOUND_SESSIONS` — the `IN (…)` list ceiling before the subquery arm.
pub const MAX_BOUND_SESSIONS: usize = 900;

/// `_LATENCY_LEAD_SQL` — `{scope}` is spliced with an *uncorrelated* predicate.
///
/// Reproduced shape for shape, including the
/// `LEAD(timestamp) OVER (PARTITION BY session_fk ORDER BY seq)` window and the
/// trailing `AND id >= ?` hoisted floor. See the module docs for what the query
/// plan looks like when either half is wrong.
pub const LATENCY_LEAD_SQL: &str = "SELECT id, \
                                    timestamp AS msg_ts, \
                                    LEAD(timestamp) OVER (\
                                    PARTITION BY session_fk ORDER BY seq\
                                    ) AS next_ts \
                                    FROM messages \
                                    WHERE session_fk IN ({scope}) \
                                    AND id >= ?";

/// `datetime.min` as epoch microseconds — `0001-01-01T00:00:00.000000`.
const DATETIME_MIN_US: i64 = -62_135_596_800_000_000;

/// `datetime.max` as epoch microseconds — `9999-12-31T23:59:59.999999`.
const DATETIME_MAX_US: i64 = 253_402_300_799_999_999;

/// `timedelta`'s own ceiling: `days` must have magnitude ≤ 999_999_999.
///
/// `timedelta(minutes=n)` raises `OverflowError` before the addition even
/// happens once `n` exceeds this. Same status code as the range check below, so
/// the distinction never reaches the wire — but it is a *different* raise, and
/// the guard belongs where Python's does.
const TIMEDELTA_MAX_MINUTES: i64 = 999_999_999 * 24 * 60;

/// What went wrong inside a snapshot. Both arms are a `500`.
#[derive(Debug)]
pub enum LiveError {
    /// CPython's `OverflowError: date value out of range` — an unclamped
    /// `timezone_offset` pushed the local wall clock out of `datetime`'s range.
    DateOverflow,
    /// Any SQLite failure. Python lets it escape the handler too.
    Sql(rusqlite::Error),
}

impl From<rusqlite::Error> for LiveError {
    fn from(err: rusqlite::Error) -> Self {
        Self::Sql(err)
    }
}

impl std::fmt::Display for LiveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DateOverflow => f.write_str("date value out of range"),
            Self::Sql(err) => write!(f, "{err}"),
        }
    }
}

// ── small utilities ──────────────────────────────────────────────────────────

/// `_iso_to_dt` — and note how it differs from `aggregator._parse_ts`.
///
/// Two deliberate differences from [`pydatetime::parse_ts`], which is why this
/// is not a call straight through to it:
///
/// 1. the `Z` → `+00:00` rewrite is **conditional on the string ending in `Z`**
///    (`_parse_ts` rewrites unconditionally), so a stray interior `Z` reaches
///    `fromisoformat` unmodified and raises — `None` here;
/// 2. a naive value is given `tzinfo=UTC` (`.replace`, so the wall clock does
///    not move), where `_parse_ts` leaves it naive. Every consumer here
///    subtracts two of these, and CPython raises `TypeError` on mixed
///    awareness — this fixup is what makes that unreachable.
#[must_use]
pub fn iso_to_dt(iso_ts: &str) -> Option<PyDateTime> {
    // `if not iso_ts:` — truthiness, so the empty string is `None`.
    if iso_ts.is_empty() {
        return None;
    }
    let norm = if iso_ts.ends_with('Z') {
        // `str.replace` is global: a value ending in `Z` has EVERY `Z` rewritten.
        iso_ts.replace('Z', "+00:00")
    } else {
        iso_ts.to_owned()
    };
    if norm.contains('Z') {
        // Not a trailing `Z`, so Python handed the raw string to
        // `fromisoformat`, which raised `ValueError`.
        return None;
    }
    let parsed = pydatetime::parse_ts(&norm)?;
    Some(PyDateTime {
        wall_us: parsed.wall_us,
        // `ts.replace(tzinfo=UTC)` — the offset is set, the wall clock is not.
        offset_s: Some(parsed.offset_s.unwrap_or(0)),
    })
}

// ── max-id watermarks (for the SSE seed) ─────────────────────────────────────

/// `max_event_id` — `MAX(usage_events.id)`, or 0 on an empty / missing table.
///
/// # Errors
/// Any SQLite error.
pub fn max_event_id(conn: &Connection) -> rusqlite::Result<i64> {
    max_id(conn, "usage_events")
}

/// `max_tool_call_id` — `MAX(message_tool_mart.id)`, or 0 on an absent mart.
///
/// # Errors
/// Any SQLite error.
pub fn max_tool_call_id(conn: &Connection) -> rusqlite::Result<i64> {
    max_id(conn, "message_tool_mart")
}

/// The body both watermark readers share, spelled once.
///
/// `int(val) if val is not None else 0` — an empty table's `MAX()` is `NULL`,
/// which lands on the same 0 a missing table gives.
fn max_id(conn: &Connection, table: &str) -> rusqlite::Result<i64> {
    if !table_exists(conn, table)? {
        return Ok(0);
    }
    let value: Option<i64> =
        conn.query_row(&format!("SELECT MAX(id) FROM {table}"), [], |row| {
            row.get(0)
        })?;
    Ok(value.unwrap_or(0))
}

// ── incremental readers ──────────────────────────────────────────────────────

/// `recent_events` — the NEWEST `limit` rows above `since_id`, re-sorted oldest
/// first.
///
/// The `ORDER BY id DESC LIMIT ?` + reverse is the skip-ahead contract, not an
/// accident: a large backlog yields its newest page and the intermediate rows
/// are deliberately dropped, because the live tab is a tail and the client ring
/// buffer keeps 100 rows.
///
/// Each row is a JSON object in **SELECT column order**, which is what
/// `dict(sqlite3.Row)` produces and therefore the order the SSE payload renders.
///
/// # Errors
/// Any SQLite error.
pub fn recent_events(
    conn: &Connection,
    since_id: i64,
    limit: i64,
) -> rusqlite::Result<Vec<Map<String, Value>>> {
    if !table_exists(conn, "usage_events")? {
        return Ok(Vec::new());
    }
    let mut stmt = conn.prepare(
        "SELECT e.id, e.ts, e.project_id, e.session_id, e.model, \
                e.cost_usd, e.input_tokens, e.output_tokens, \
                e.cache_read_tokens, e.cache_create_tokens, \
                e.cost_source, p.slug AS project_slug, \
                p.display_name AS project_name \
           FROM usage_events e \
           LEFT JOIN projects p ON p.id = e.project_id \
          WHERE e.id > ? \
          ORDER BY e.id DESC \
          LIMIT ?",
    )?;
    let mut rows = stmt.query(rusqlite::params![since_id, limit])?;
    let mut out = collect_rows(&mut rows)?;
    // `reversed(rows)` — ascending by id, so the caller's `max()` lands on the
    // true maximum and the UI's merge stays sorted.
    out.reverse();
    Ok(out)
}

/// `recent_tool_calls` — the same skip-ahead contract over `message_tool_mart`.
///
/// # Errors
/// Any SQLite error.
pub fn recent_tool_calls(
    conn: &Connection,
    since_id: i64,
    limit: i64,
) -> rusqlite::Result<Vec<Map<String, Value>>> {
    if !table_exists(conn, "message_tool_mart")? {
        return Ok(Vec::new());
    }
    let mut stmt = conn.prepare(
        "SELECT t.id, t.ts, t.project_id, t.session_id, t.tool_name, \
                t.file_path, t.byte_count, t.call_index, \
                p.slug AS project_slug, p.display_name AS project_name \
           FROM message_tool_mart t \
           LEFT JOIN projects p ON p.id = t.project_id \
          WHERE t.id > ? \
          ORDER BY t.id DESC \
          LIMIT ?",
    )?;
    let mut rows = stmt.query(rusqlite::params![since_id, limit])?;
    let mut out = collect_rows(&mut rows)?;
    out.reverse();
    Ok(out)
}

/// `[dict(r) for r in rows]` — column name → value, in SELECT order.
///
/// `sqlite3.Row` keys on the *alias*, so `p.slug AS project_slug` is
/// `project_slug`; rusqlite's `column_names` gives the same list.
fn collect_rows(rows: &mut rusqlite::Rows<'_>) -> rusqlite::Result<Vec<Map<String, Value>>> {
    let names: Vec<String> = rows
        .as_ref()
        .map(|stmt| stmt.column_names().into_iter().map(str::to_owned).collect())
        .unwrap_or_default();
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        let mut obj = Map::new();
        for (index, name) in names.iter().enumerate() {
            obj.insert(name.clone(), sql_to_json(row.get_ref(index)?));
        }
        out.push(obj);
    }
    Ok(out)
}

/// One `sqlite3` column value, as `json.dumps(…, default=str)` would write it.
fn sql_to_json(value: rusqlite::types::ValueRef<'_>) -> Value {
    use rusqlite::types::ValueRef;
    match value {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(int) => Value::from(int),
        ValueRef::Real(real) => Value::from(real),
        ValueRef::Text(bytes) => Value::from(String::from_utf8_lossy(bytes).into_owned()),
        // `default=str` on a `bytes` renders its `repr`. No column selected
        // here is a BLOB, so this arm exists to be total, not to be reached.
        ValueRef::Blob(bytes) => Value::from(format!("{bytes:?}")),
    }
}

// ── burn rate ────────────────────────────────────────────────────────────────

/// `_day_str` — the `YYYY-MM-DD` key the `idx_events_day` prefilter compares.
///
/// `strftime` on an aware value reads the **wall clock as written**, so this is
/// a pure civil-calendar rendering of the microsecond stamp, with no offset
/// normalisation. Every value it is handed here already sits in the UTC frame.
#[must_use]
pub fn day_str(micros: i64) -> String {
    let (year, month, day, ..) = pydatetime::civil_from_epoch(micros.div_euclid(1_000_000));
    format!("{year:04}-{month:02}-{day:02}")
}

/// `now + timedelta(minutes=minutes)`, with CPython's two `OverflowError`s.
///
/// `None` is the raise: either the `timedelta` itself is out of range
/// (magnitude over 999_999_999 days) or the resulting `datetime` leaves
/// `[datetime.min, datetime.max]`.
fn plus_minutes_checked(micros: i64, minutes: i64) -> Option<i64> {
    if minutes.checked_abs()? > TIMEDELTA_MAX_MINUTES {
        return None;
    }
    let shifted = micros.checked_add(minutes.checked_mul(60_000_000)?)?;
    (DATETIME_MIN_US..=DATETIME_MAX_US)
        .contains(&shifted)
        .then_some(shifted)
}

/// The four cutoffs `_burn_cutoffs` returns, as epoch microseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cutoffs {
    /// `now - timedelta(minutes=window_minutes)`.
    pub window_us: i64,
    /// The UTC instant of the local day start.
    pub today_us: i64,
    /// The UTC instant of the local month start.
    pub month_us: i64,
    /// `now + timedelta(minutes=tz_offset)` — the local wall clock.
    pub local_now_us: i64,
}

/// `_burn_cutoffs` — `(window, today, month, local_now)`, all in the UTC frame.
///
/// `tz_offset` is minutes *added* to a UTC timestamp to reach local wall-clock
/// time (`aggregator._local_day`'s convention). The `today` / `month` values
/// come back as the UTC instants of the local day/month start, so they compare
/// directly against the stored ISO-8601 `ts` strings.
///
/// `None` reproduces the `OverflowError` — see the module docs.
#[must_use]
pub fn burn_cutoffs(now_us: i64, window_minutes: i64, tz_offset: i64) -> Option<Cutoffs> {
    let local_now = plus_minutes_checked(now_us, tz_offset)?;
    // `.replace(hour=0, minute=0, second=0, microsecond=0)` — floor to midnight
    // on the WALL clock, which `div_euclid` does correctly for pre-epoch values.
    let day_us = 86_400 * 1_000_000;
    let local_today = local_now.div_euclid(day_us) * day_us;
    // `.replace(day=1)`: step back `day - 1` whole days. No `days_from_civil`
    // needed and no month-length table — the calendar cannot move under us
    // inside a single month.
    let (_, _, day_of_month, ..) = pydatetime::civil_from_epoch(local_today.div_euclid(1_000_000));
    let local_month = local_today - (day_of_month - 1) * day_us;
    Some(Cutoffs {
        window_us: plus_minutes_checked(now_us, -window_minutes)?,
        today_us: plus_minutes_checked(local_today, -tz_offset)?,
        month_us: plus_minutes_checked(local_month, -tz_offset)?,
        local_now_us: local_now,
    })
}

/// `_window_cost` — always live, never cached, bounded by `idx_events_day`.
///
/// # Errors
/// Any SQLite error.
pub fn window_cost(conn: &Connection, window_us: i64) -> rusqlite::Result<f64> {
    let total: Option<f64> = conn.query_row(
        "SELECT SUM(cost_usd) FROM usage_events WHERE day >= ? AND ts >= ?",
        rusqlite::params![day_str(window_us), pytime::isoformat_utc(window_us)],
        |row| row.get(0),
    )?;
    // `float(row[0] or 0.0)`.
    Ok(total.unwrap_or(0.0))
}

/// The per-stream today/MTD memo `rolling_burn` optionally threads through.
///
/// The SSE loop owns one of these for the life of a connection;
/// `/api/live/stats` passes `None` and therefore always reads fresh.
#[derive(Debug, Clone, Default)]
pub struct BurnCache {
    /// `cache["today_month"]` — `(key, cached_at_us, today_cost, month_cost)`.
    today_month: Option<((String, String), i64, f64, f64)>,
}

/// `_today_month_cost` — one `idx_events_day`-bounded scan, optionally memoized.
///
/// The cache key is the `(today_cutoff, month_cutoff)` ISO pair, so crossing
/// local midnight or the 1st invalidates it without a timer; the TTL is the
/// second gate. `0 <= age < TTL` — an entry stamped in the *future* (a clock
/// step backwards) is discarded rather than trusted.
///
/// # Errors
/// Any SQLite error.
pub fn today_month_cost(
    conn: &Connection,
    today_us: i64,
    month_us: i64,
    now_us: i64,
    cache: Option<&mut BurnCache>,
) -> rusqlite::Result<(f64, f64)> {
    let today_iso = pytime::isoformat_utc(today_us);
    let month_iso = pytime::isoformat_utc(month_us);
    let key = (today_iso.clone(), month_iso.clone());

    if let Some(cache) = cache.as_ref()
        && let Some((ckey, cached_at, ctoday, cmonth)) = cache.today_month.as_ref()
    {
        #[allow(
            clippy::cast_precision_loss,
            reason = "(now - cached_at).total_seconds() — CPython divides the exact microsecond delta by 1e6"
        )]
        let age = (now_us - cached_at) as f64 / 1_000_000.0;
        if *ckey == key && (0.0..BURN_TODAY_CACHE_TTL_SECONDS).contains(&age) {
            return Ok((*ctoday, *cmonth));
        }
    }

    let (today_cost, month_cost) = conn.query_row(
        "SELECT \
           SUM(CASE WHEN ts >= ? THEN cost_usd ELSE 0 END) AS today_cost, \
           SUM(CASE WHEN ts >= ? THEN cost_usd ELSE 0 END) AS month_cost \
         FROM usage_events WHERE day >= ?",
        rusqlite::params![today_iso, month_iso, day_str(month_us)],
        |row| {
            Ok((
                row.get::<_, Option<f64>>(0)?.unwrap_or(0.0),
                row.get::<_, Option<f64>>(1)?.unwrap_or(0.0),
            ))
        },
    )?;
    if let Some(cache) = cache {
        cache.today_month = Some((key, now_us, today_cost, month_cost));
    }
    Ok((today_cost, month_cost))
}

/// The `rolling_burn` dict. Field order is the dict-literal's order.
#[derive(Debug, Clone, PartialEq)]
pub struct Burn {
    /// `window_minutes` — an **int** in the payload, not a float.
    pub window_minutes: i64,
    /// `window_cost` — the last-N-minute total.
    pub window_cost: f64,
    /// `per_minute` — `window_cost / max(window_minutes, 1)`.
    pub per_minute: f64,
    /// `per_hour` — `per_minute * 60.0`.
    pub per_hour: f64,
    /// `today_cost` — since the *local* midnight.
    pub today_cost: f64,
    /// `month_to_date` — since the *local* month start.
    pub month_to_date: f64,
    /// `projected_month_end` — `MTD + avg_daily * days_left`.
    pub projected_month_end: f64,
    /// `ts` — the ISO instant the snapshot was taken. **This is the clock stamp
    /// that keeps `!LV-stats` open; see `parity/DIV-e-live.md`.**
    pub ts: String,
}

impl Burn {
    /// The dict, in the literal's key order.
    #[must_use]
    pub fn to_value(&self) -> Value {
        let mut obj = Map::new();
        obj.insert(
            "window_minutes".to_owned(),
            Value::from(self.window_minutes),
        );
        obj.insert("window_cost".to_owned(), Value::from(self.window_cost));
        obj.insert("per_minute".to_owned(), Value::from(self.per_minute));
        obj.insert("per_hour".to_owned(), Value::from(self.per_hour));
        obj.insert("today_cost".to_owned(), Value::from(self.today_cost));
        obj.insert("month_to_date".to_owned(), Value::from(self.month_to_date));
        obj.insert(
            "projected_month_end".to_owned(),
            Value::from(self.projected_month_end),
        );
        obj.insert("ts".to_owned(), Value::from(self.ts.clone()));
        Value::Object(obj)
    }
}

/// `rolling_burn` — the window / today / MTD / projection block.
///
/// `now` of `None` is `_now_utc()`. Note the ORDER of the two guards: the
/// cutoffs are computed **before** the `usage_events` table check, so an
/// out-of-range `tz_offset` raises even on a store with no events at all. That
/// ordering is load-bearing for the `500` and is reproduced as written.
///
/// # Errors
/// [`LiveError::DateOverflow`] for the raise, or any SQLite error.
pub fn rolling_burn(
    conn: &Connection,
    window_minutes: i64,
    now: Option<i64>,
    tz_offset: i64,
    cache: Option<&mut BurnCache>,
) -> Result<Burn, LiveError> {
    let now_us = now.unwrap_or_else(pytime::now_micros);
    let cutoffs = burn_cutoffs(now_us, window_minutes, tz_offset).ok_or(LiveError::DateOverflow)?;
    let ts = pytime::isoformat_utc(now_us);

    if !table_exists(conn, "usage_events")? {
        return Ok(Burn {
            window_minutes,
            window_cost: 0.0,
            per_minute: 0.0,
            per_hour: 0.0,
            today_cost: 0.0,
            month_to_date: 0.0,
            projected_month_end: 0.0,
            ts,
        });
    }

    let window_total = window_cost(conn, cutoffs.window_us)?;
    let (today_cost, month_cost) =
        today_month_cost(conn, cutoffs.today_us, cutoffs.month_us, now_us, cache)?;

    #[allow(
        clippy::cast_precision_loss,
        reason = "max(window_minutes, 1) — a minute count, far under 2^53"
    )]
    let per_minute = window_total / window_minutes.max(1) as f64;
    let per_hour = per_minute * 60.0;

    // `calendar.monthrange(local_now.year, local_now.month)[1]`, on the LOCAL
    // calendar so "days so far" / "days left" match the local-midnight buckets.
    let (year, month, day, ..) =
        pydatetime::civil_from_epoch(cutoffs.local_now_us.div_euclid(1_000_000));
    #[allow(
        clippy::cast_sign_loss,
        clippy::cast_possible_truncation,
        reason = "civil_from_epoch yields a calendar month in 1..=12"
    )]
    let month_length = i64::from(days_in_month(year, month as u32));
    // `max(local_now.day, 1)` — the day-1 divide-by-zero guard, kept even
    // though `.day` is never below 1.
    let days_so_far = day.max(1);
    let days_left = (month_length - day).max(0);
    #[allow(
        clippy::cast_precision_loss,
        reason = "day counts inside one calendar month"
    )]
    let avg_daily = month_cost / days_so_far as f64;
    #[allow(
        clippy::cast_precision_loss,
        reason = "day counts inside one calendar month"
    )]
    let projected = month_cost + avg_daily * days_left as f64;

    Ok(Burn {
        window_minutes,
        window_cost: window_total,
        per_minute,
        per_hour,
        today_cost,
        month_to_date: month_cost,
        projected_month_end: projected,
        ts,
    })
}

// ── tool latency percentiles ─────────────────────────────────────────────────

/// `_percentile` — nearest-rank on a pre-sorted list, `p` ∈ [0, 100].
///
/// The Python docstring promises `ceil(p/100 * N) - 1`; the code does
/// `int((p / 100.0) * N)` clamped, and the code is what ships. Evaluated in
/// `f64` so the binary rounding matches — `int(0.95 * 61)` is 57.
#[must_use]
pub fn percentile(sorted_values: &[f64], p: f64) -> f64 {
    if sorted_values.is_empty() {
        return 0.0;
    }
    if sorted_values.len() == 1 {
        return sorted_values[0];
    }
    #[allow(
        clippy::cast_precision_loss,
        reason = "len() of one tool's sample list"
    )]
    let count = sorted_values.len() as f64;
    let raw = (p / 100.0) * count;
    // `int(x)` truncates toward zero; `raw` is non-negative for every `p` this
    // module passes, and the clamp is Python's `max(0, min(N - 1, …))`.
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "int(float) — the clamp below bounds the result either way"
    )]
    let index = if raw <= 0.0 { 0 } else { raw as usize };
    sorted_values[index.min(sorted_values.len() - 1)]
}

/// `{tool_name: [latency_seconds, ...]}` — a `Vec` because the ORDER is the
/// contract (see the module docs on the stable sort).
type Samples = Vec<(String, Vec<f64>)>;

/// `_latency_samples` — the two-statement window query.
///
/// `now_us` stands in for the `_now_utc()` call Python makes *here*; it is a
/// SECOND clock read, independent of `rolling_burn`'s, which is why
/// [`snapshot`] does not thread one value through both.
///
/// # Errors
/// Any SQLite error.
pub fn latency_samples(
    conn: &Connection,
    window_hours: i64,
    now_us: i64,
) -> rusqlite::Result<Samples> {
    if !table_exists(conn, "message_tool_mart")? {
        return Ok(Vec::new());
    }
    let cutoff = pytime::isoformat_utc(now_us - window_hours * 3_600 * 1_000_000);

    // `win`: every in-window mart row. Order is SQLite's, and it decides the
    // insertion order of the output map.
    let mut stmt = conn
        .prepare("SELECT message_id, tool_name, session_id FROM message_tool_mart WHERE ts >= ?")?;
    let win: Vec<(i64, Option<String>, Option<String>)> = stmt
        .query_map([&cutoff], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
        .collect::<rusqlite::Result<_>>()?;
    if win.is_empty() {
        // Cold-window fast path: no floor, nothing to scope by, no LEAD.
        return Ok(Vec::new());
    }

    let min_id = win.iter().map(|row| row.0).min().unwrap_or(0);
    // `sorted({r[2] for r in win if r[2]})` — a SET, so duplicates collapse;
    // `if r[2]` is truthiness, so NULL and "" are both dropped. The sort makes
    // the bound-parameter order reproducible; it cannot change the result.
    let mut session_ids: Vec<String> = win
        .iter()
        .filter_map(|row| row.2.clone().filter(|value| !value.is_empty()))
        .collect();
    session_ids.sort_unstable();
    session_ids.dedup();
    if session_ids.is_empty() {
        return Ok(Vec::new());
    }

    let mut params: Vec<rusqlite::types::Value> = Vec::new();
    let scope = if session_ids.len() <= MAX_BOUND_SESSIONS {
        let placeholders = vec!["?"; session_ids.len()].join(",");
        for id in &session_ids {
            params.push(rusqlite::types::Value::Text(id.clone()));
        }
        format!("SELECT id FROM sessions WHERE session_id IN ({placeholders})")
    } else {
        // Pathological window: keep the session filter in SQL as an
        // uncorrelated subquery over the mart — still evaluated once.
        params.push(rusqlite::types::Value::Text(cutoff.clone()));
        "SELECT id FROM sessions WHERE session_id IN \
         (SELECT session_id FROM message_tool_mart WHERE ts >= ?)"
            .to_owned()
    };
    params.push(rusqlite::types::Value::Integer(min_id));

    let sql = LATENCY_LEAD_SQL.replace("{scope}", &scope);
    let mut lead = conn.prepare(&sql)?;
    let mut pairs: std::collections::HashMap<i64, (Option<String>, Option<String>)> =
        std::collections::HashMap::new();
    let mut rows = lead.query(rusqlite::params_from_iter(params.iter()))?;
    while let Some(row) = rows.next()? {
        pairs.insert(row.get(0)?, (row.get(1)?, row.get(2)?));
    }

    // `out.setdefault(name, []).append(delta)` — insertion-ordered by the first
    // appearance of each tool in the `win` scan.
    let mut order: Vec<String> = Vec::new();
    let mut buckets: Vec<Vec<f64>> = Vec::new();
    for (message_id, tool_name, _session_id) in &win {
        // Inner-join semantics: a mart row whose source message is gone
        // contributes nothing, exactly as the old SQL JOIN did.
        let Some((msg_ts, next_ts)) = pairs.get(message_id) else {
            continue;
        };
        // `name = tool_name or ""` then `if not name: continue`.
        let name = tool_name.clone().unwrap_or_default();
        if name.is_empty() {
            continue;
        }
        let (Some(start), Some(end)) = (
            msg_ts.as_deref().and_then(iso_to_dt),
            next_ts.as_deref().and_then(iso_to_dt),
        ) else {
            continue;
        };
        let Some(delta) = end.sub_total_seconds(start) else {
            continue;
        };
        // Clock skew on imported logs puts the next message BEFORE the
        // tool_use. Dropped rather than allowed to poison the percentile.
        if delta < 0.0 {
            continue;
        }
        match order.iter().position(|seen| *seen == name) {
            Some(index) => buckets[index].push(delta),
            None => {
                order.push(name);
                buckets.push(vec![delta]);
            }
        }
    }
    Ok(order.into_iter().zip(buckets).collect())
}

/// One row of the latency table.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolLatency {
    /// `tool_name`.
    pub tool_name: String,
    /// `samples` — the count that also decides the sort.
    pub samples: i64,
    /// `p50` — seconds.
    pub p50: f64,
    /// `p95` — seconds.
    pub p95: f64,
    /// `p99` — seconds.
    pub p99: f64,
}

impl ToolLatency {
    /// The dict, in the literal's key order.
    #[must_use]
    pub fn to_value(&self) -> Value {
        let mut obj = Map::new();
        obj.insert("tool_name".to_owned(), Value::from(self.tool_name.clone()));
        obj.insert("samples".to_owned(), Value::from(self.samples));
        obj.insert("p50".to_owned(), Value::from(self.p50));
        obj.insert("p95".to_owned(), Value::from(self.p95));
        obj.insert("p99".to_owned(), Value::from(self.p99));
        Value::Object(obj)
    }
}

/// `tool_latency_percentiles` — per-tool P50/P95/P99, busiest first.
///
/// `now_us` is the injected `_now_utc()`; [`snapshot`] reads the clock here
/// separately from `rolling_burn`'s read, because Python does.
///
/// # Errors
/// Any SQLite error.
pub fn tool_latency_percentiles(
    conn: &Connection,
    window_hours: i64,
    top_n: i64,
    now_us: i64,
) -> rusqlite::Result<Vec<ToolLatency>> {
    let samples = latency_samples(conn, window_hours, now_us)?;
    let mut out: Vec<ToolLatency> = samples
        .into_iter()
        .map(|(tool, mut vals)| {
            // `sorted(vals)` — every value is a finite non-negative delta.
            vals.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            ToolLatency {
                tool_name: tool,
                samples: i64::try_from(vals.len()).unwrap_or(i64::MAX),
                p50: percentile(&vals, 50.0),
                p95: percentile(&vals, 95.0),
                p99: percentile(&vals, 99.0),
            }
        })
        .collect();
    // `out.sort(key=lambda x: -x["samples"])` — STABLE, so equal counts keep
    // their first-appearance order.
    out.sort_by_key(|entry| std::cmp::Reverse(entry.samples));
    // `out[: max(0, int(top_n))]`.
    let keep = usize::try_from(top_n.max(0)).unwrap_or(usize::MAX);
    out.truncate(keep);
    Ok(out)
}

// ── snapshot for /api/live/stats ─────────────────────────────────────────────

/// `snapshot` — the burn block, the latency table, and the two watermarks.
///
/// Three keys, in the dict-literal's order. The route adds a fourth
/// (`watcher`) after this returns, which is why it is not here.
///
/// # Errors
/// [`LiveError::DateOverflow`] for the unclamped-offset raise, or any SQLite
/// error.
pub fn snapshot(
    conn: &Connection,
    burn_window_minutes: i64,
    latency_window_hours: i64,
    top_tools: i64,
    tz_offset: i64,
) -> Result<Value, LiveError> {
    let burn = rolling_burn(conn, burn_window_minutes, None, tz_offset, None)?;
    // A SECOND clock read — `_latency_samples` calls `_now_utc()` itself.
    let latency =
        tool_latency_percentiles(conn, latency_window_hours, top_tools, pytime::now_micros())?;

    let mut watermarks = Map::new();
    watermarks.insert("event_id".to_owned(), Value::from(max_event_id(conn)?));
    watermarks.insert(
        "tool_call_id".to_owned(),
        Value::from(max_tool_call_id(conn)?),
    );

    let mut out = Map::new();
    out.insert("burn".to_owned(), burn.to_value());
    out.insert(
        "tool_latency".to_owned(),
        Value::Array(latency.iter().map(ToolLatency::to_value).collect()),
    );
    out.insert("watermarks".to_owned(), Value::Object(watermarks));
    Ok(Value::Object(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A miniature of the shared store: three monthly partitions behind a
    /// UNION-ALL `messages` VIEW, each with the `(session_fk, seq)` index the
    /// list-subquery shape is supposed to reach.
    const SCHEMA: &str = "
        CREATE TABLE projects (
          id INTEGER PRIMARY KEY, provider TEXT NOT NULL, slug TEXT NOT NULL,
          display_name TEXT NOT NULL);
        CREATE TABLE sessions (
          id INTEGER PRIMARY KEY, project_id INTEGER NOT NULL,
          session_id TEXT NOT NULL, UNIQUE (project_id, session_id));
        CREATE TABLE usage_events (
          id INTEGER PRIMARY KEY, project_id INTEGER NOT NULL,
          session_id TEXT NOT NULL, ts TEXT NOT NULL, day TEXT NOT NULL,
          model TEXT NOT NULL DEFAULT '', cost_usd REAL NOT NULL DEFAULT 0.0,
          input_tokens INTEGER NOT NULL DEFAULT 0,
          output_tokens INTEGER NOT NULL DEFAULT 0,
          cache_read_tokens INTEGER NOT NULL DEFAULT 0,
          cache_create_tokens INTEGER NOT NULL DEFAULT 0,
          cost_source TEXT NOT NULL DEFAULT 'rate_card');
        CREATE INDEX idx_events_day ON usage_events(day);
        CREATE TABLE message_tool_mart (
          id INTEGER PRIMARY KEY, message_id INTEGER NOT NULL,
          project_id INTEGER NOT NULL, session_id TEXT NOT NULL,
          ts TEXT NOT NULL, day TEXT NOT NULL, tool_name TEXT NOT NULL,
          file_path TEXT, byte_count INTEGER, call_index INTEGER);
        CREATE TABLE messages_202605 (
          id INTEGER PRIMARY KEY, session_fk INTEGER NOT NULL, seq INTEGER NOT NULL,
          timestamp TEXT NOT NULL, role TEXT NOT NULL, UNIQUE (session_fk, seq));
        CREATE TABLE messages_202606 (
          id INTEGER PRIMARY KEY, session_fk INTEGER NOT NULL, seq INTEGER NOT NULL,
          timestamp TEXT NOT NULL, role TEXT NOT NULL, UNIQUE (session_fk, seq));
        CREATE TABLE messages_202607 (
          id INTEGER PRIMARY KEY, session_fk INTEGER NOT NULL, seq INTEGER NOT NULL,
          timestamp TEXT NOT NULL, role TEXT NOT NULL, UNIQUE (session_fk, seq));
        CREATE VIEW messages AS
          SELECT id, session_fk, seq, timestamp, role FROM messages_202605
          UNION ALL
          SELECT id, session_fk, seq, timestamp, role FROM messages_202606
          UNION ALL
          SELECT id, session_fk, seq, timestamp, role FROM messages_202607;
        CREATE INDEX idx_messages_202605_session_seq ON messages_202605(session_fk, seq);
        CREATE INDEX idx_messages_202606_session_seq ON messages_202606(session_fk, seq);
        CREATE INDEX idx_messages_202607_session_seq ON messages_202607(session_fk, seq);
    ";

    fn fixture() -> Connection {
        let conn = Connection::open_in_memory().expect("in-memory store");
        conn.execute_batch(SCHEMA).expect("schema");
        conn
    }

    /// One project, two sessions, and a handful of messages + mart rows whose
    /// deltas are hand-checkable.
    fn seed(conn: &Connection) {
        conn.execute_batch(
            "
            INSERT INTO projects VALUES (1, 'claude', 'demo', 'Demo');
            INSERT INTO sessions VALUES (1, 1, 's-one'), (2, 1, 's-two');

            -- s-one lives in 202607, s-two in 202606: two partitions in play.
            INSERT INTO messages_202607 VALUES
              (10, 1, 0, '2026-07-30T10:00:00+00:00', 'assistant'),
              (11, 1, 1, '2026-07-30T10:00:02+00:00', 'user'),
              (12, 1, 2, '2026-07-30T10:00:03+00:00', 'assistant'),
              (13, 1, 3, '2026-07-30T10:00:13+00:00', 'user');
            INSERT INTO messages_202606 VALUES
              (20, 2, 0, '2026-06-30T10:00:00+00:00', 'assistant'),
              (21, 2, 1, '2026-06-30T10:00:01+00:00', 'user');

            INSERT INTO message_tool_mart
              (id, message_id, project_id, session_id, ts, day, tool_name,
               file_path, byte_count, call_index) VALUES
              (1, 10, 1, 's-one', '2026-07-30T10:00:00+00:00', '2026-07-30', 'Bash', NULL, NULL, 0),
              (2, 12, 1, 's-one', '2026-07-30T10:00:03+00:00', '2026-07-30', 'Bash', NULL, NULL, 1),
              (3, 20, 1, 's-two', '2026-07-30T10:00:00+00:00', '2026-07-30', 'Read', '/a', 12, 0);

            INSERT INTO usage_events
              (id, project_id, session_id, ts, day, model, cost_usd) VALUES
              (1, 1, 's-one', '2026-07-30T10:00:00+00:00', '2026-07-30', 'opus', 1.5),
              (2, 1, 's-one', '2026-07-31T09:00:00+00:00', '2026-07-31', 'opus', 2.25),
              (3, 1, 's-one', '2026-07-31T11:59:00+00:00', '2026-07-31', 'opus', 0.25);
            ",
        )
        .expect("seed");
    }

    /// `2026-07-31T12:00:00+00:00` in epoch microseconds — the clock every
    /// arithmetic test injects.
    const NOON: i64 = 1_785_499_200_000_000;

    #[test]
    fn the_injected_clock_constant_is_the_instant_it_claims_to_be() {
        assert_eq!(pytime::isoformat_utc(NOON), "2026-07-31T12:00:00+00:00");
        // …and the two `datetime` bounds are the ones CPython enforces.
        assert_eq!(
            pytime::isoformat_utc(DATETIME_MIN_US),
            "0001-01-01T00:00:00+00:00"
        );
        assert_eq!(
            pytime::isoformat_utc(DATETIME_MAX_US),
            "9999-12-31T23:59:59.999999+00:00"
        );
    }

    // ── the plan assertion (finding 10 in ARCHITECT-STATE.md) ────────────────

    /// The `EXPLAIN QUERY PLAN` detail column, one string per node.
    fn plan(conn: &Connection, sql: &str, params: &[rusqlite::types::Value]) -> Vec<String> {
        let mut stmt = conn
            .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
            .expect("preparing the plan");
        stmt.query_map(rusqlite::params_from_iter(params.iter()), |row| {
            row.get::<_, String>(3)
        })
        .expect("running the plan")
        .collect::<rusqlite::Result<Vec<String>>>()
        .expect("collecting the plan")
    }

    #[test]
    fn the_latency_plan_searches_every_partition_and_hoists_the_session_list() {
        let conn = fixture();
        seed(&conn);

        let scope = "SELECT id FROM sessions WHERE session_id IN (?,?)";
        let sql = LATENCY_LEAD_SQL.replace("{scope}", scope);
        let params = [
            rusqlite::types::Value::Text("s-one".to_owned()),
            rusqlite::types::Value::Text("s-two".to_owned()),
            rusqlite::types::Value::Integer(10),
        ];
        let detail = plan(&conn, &sql, &params);

        // (a) PARTITION-LOCALITY. Every arm SEARCHes its (session_fk, seq)
        // index. A single `SCAN messages_<ym>` here is the July hang.
        let searches = detail
            .iter()
            .filter(|line| line.starts_with("SEARCH messages_2"))
            .count();
        assert_eq!(
            searches, 3,
            "every UNION-ALL arm must SEARCH its session index; plan was {detail:#?}"
        );
        assert!(
            !detail
                .iter()
                .any(|line| line.starts_with("SCAN messages_2")),
            "no partition may be SCANned; plan was {detail:#?}"
        );
        assert!(
            detail
                .iter()
                .filter(|line| line.starts_with("SEARCH messages_2"))
                .all(|line| line.contains("(session_fk=?)")),
            "the SEARCH must be driven by session_fk; plan was {detail:#?}"
        );

        // (b) THE HOISTED LIST. The session predicate is evaluated ONCE and
        // reused by every remaining arm — that is what "uncorrelated" buys.
        assert_eq!(
            detail
                .iter()
                .filter(|line| line.contains("LIST SUBQUERY") && !line.contains("REUSE"))
                .count(),
            1,
            "the session list must be built once; plan was {detail:#?}"
        );
        assert_eq!(
            detail
                .iter()
                .filter(|line| line.contains("REUSE LIST SUBQUERY"))
                .count(),
            2,
            "the remaining arms must REUSE it; plan was {detail:#?}"
        );
    }

    #[test]
    fn the_pre_fix_scalar_subquery_floor_scans_every_partition() {
        // The counterfactual, so the assertion above is known to DISCRIMINATE
        // rather than to pass on any query at all. This is the shape
        // `_latency_samples` replaced: a scalar subquery inside the view, which
        // SQLite re-evaluates per UNION-ALL arm, and an `id >=` row filter that
        // prunes nothing.
        let conn = fixture();
        seed(&conn);
        let old = "SELECT id, timestamp AS msg_ts, \
                          LEAD(timestamp) OVER (PARTITION BY session_fk ORDER BY seq) AS next_ts \
                     FROM messages \
                    WHERE id >= (SELECT MIN(message_id) FROM message_tool_mart WHERE ts >= ?)";
        let detail = plan(
            &conn,
            old,
            &[rusqlite::types::Value::Text(
                "2026-07-01T00:00:00+00:00".to_owned(),
            )],
        );
        assert_eq!(
            detail
                .iter()
                .filter(|line| line.starts_with("SCAN messages_2"))
                .count(),
            3,
            "the old shape SCANs every arm — that is the regression under test; plan was {detail:#?}"
        );
    }

    #[test]
    fn the_two_scope_shapes_return_the_same_rows() {
        // The `> _MAX_BOUND_SESSIONS` fallback is not reachable with two
        // sessions, so drive it directly and prove it is the same query.
        let conn = fixture();
        seed(&conn);
        let cutoff = "2026-06-01T00:00:00+00:00";

        let list_sql = LATENCY_LEAD_SQL.replace(
            "{scope}",
            "SELECT id FROM sessions WHERE session_id IN (?,?)",
        );
        let sub_sql = LATENCY_LEAD_SQL.replace(
            "{scope}",
            "SELECT id FROM sessions WHERE session_id IN \
             (SELECT session_id FROM message_tool_mart WHERE ts >= ?)",
        );
        let read = |sql: &str, params: Vec<rusqlite::types::Value>| -> Vec<(i64, Option<String>)> {
            let mut stmt = conn.prepare(sql).expect("prepare");
            let mut rows = stmt
                .query(rusqlite::params_from_iter(params.iter()))
                .expect("query");
            let mut out = Vec::new();
            while let Some(row) = rows.next().expect("row") {
                out.push((row.get(0).expect("id"), row.get(2).expect("next_ts")));
            }
            out.sort_unstable();
            out
        };
        let by_list = read(
            &list_sql,
            vec![
                rusqlite::types::Value::Text("s-one".to_owned()),
                rusqlite::types::Value::Text("s-two".to_owned()),
                rusqlite::types::Value::Integer(10),
            ],
        );
        let by_subquery = read(
            &sub_sql,
            vec![
                rusqlite::types::Value::Text(cutoff.to_owned()),
                rusqlite::types::Value::Integer(10),
            ],
        );
        assert_eq!(by_list, by_subquery);
        assert!(!by_list.is_empty());
    }

    // ── burn arithmetic ─────────────────────────────────────────────────────

    #[test]
    fn the_local_day_cutoffs_shift_with_the_offset_and_come_back_in_utc() {
        // Noon UTC, offset -480 (eight hours WEST despite the "east" naming —
        // this is `getTimezoneOffset()`'s sign, and the campaign inherits it):
        // the local wall clock is 04:00, so the local day started at 08:00 UTC.
        let cut = burn_cutoffs(NOON, 5, -480).expect("in range");
        assert_eq!(
            pytime::isoformat_utc(cut.local_now_us),
            "2026-07-31T04:00:00+00:00"
        );
        assert_eq!(
            pytime::isoformat_utc(cut.today_us),
            "2026-07-31T08:00:00+00:00"
        );
        assert_eq!(
            pytime::isoformat_utc(cut.month_us),
            "2026-07-01T08:00:00+00:00"
        );
        assert_eq!(
            pytime::isoformat_utc(cut.window_us),
            "2026-07-31T11:55:00+00:00"
        );

        // …and +480 lands on the NEXT local day, which is the whole point of
        // the sign mattering.
        let east = burn_cutoffs(NOON, 5, 480).expect("in range");
        assert_eq!(
            pytime::isoformat_utc(east.local_now_us),
            "2026-07-31T20:00:00+00:00"
        );
        assert_eq!(
            pytime::isoformat_utc(east.today_us),
            "2026-07-30T16:00:00+00:00"
        );
    }

    #[test]
    fn the_month_cutoff_is_the_first_of_the_local_month_not_thirty_days_back() {
        // 1 March, so a naive `.replace(day=1)` on the UTC clock would land on
        // the wrong month for a caller whose local date is still February.
        let march_first = 1_772_323_200_000_000;
        assert_eq!(
            pytime::isoformat_utc(march_first),
            "2026-03-01T00:00:00+00:00"
        );
        let west = burn_cutoffs(march_first, 5, -60).expect("in range");
        // The local wall clock is 2026-02-28T23:00, so the local month is FEBRUARY.
        assert_eq!(
            pytime::isoformat_utc(west.month_us),
            "2026-02-01T01:00:00+00:00"
        );
    }

    #[test]
    fn an_unclamped_offset_reproduces_cpythons_overflow_and_its_asymmetry() {
        // The range is not symmetric around "now": 2026 leaves ~7973 years of
        // headroom forward and only ~2026 backward, so the same magnitude
        // answers on one side and raises on the other. Measured against the
        // Python reference: `+2147483647` is 200, `-2147483648` is 500.
        assert!(burn_cutoffs(NOON, 5, 2_147_483_647).is_some());
        assert!(burn_cutoffs(NOON, 5, -2_147_483_648).is_none());
        assert!(burn_cutoffs(NOON, 5, i64::MAX).is_none());
        assert!(burn_cutoffs(NOON, 5, i64::MIN).is_none());
        // …and the values the differ actually sends are comfortably inside.
        assert!(burn_cutoffs(NOON, 5, -480).is_some());
        assert!(burn_cutoffs(NOON, 5, 100_000).is_some());
        assert!(burn_cutoffs(NOON, 5, -100_000).is_some());
    }

    #[test]
    fn the_overflow_fires_before_the_table_check_not_after() {
        // `_burn_cutoffs` runs ABOVE `if not _table_exists(...)`, so a store
        // with no `usage_events` at all still raises. Reordering the two guards
        // would turn a 500 into a 200 full of zeros.
        let conn = Connection::open_in_memory().expect("empty store");
        assert!(matches!(
            rolling_burn(&conn, 5, Some(NOON), i64::MIN, None),
            Err(LiveError::DateOverflow)
        ));
        let zeroed = rolling_burn(&conn, 5, Some(NOON), 0, None).expect("no table is not an error");
        assert_eq!(zeroed.window_minutes, 5);
        assert_eq!(zeroed.today_cost, 0.0);
        assert_eq!(zeroed.ts, "2026-07-31T12:00:00+00:00");
    }

    #[test]
    fn the_burn_block_buckets_on_the_local_day_and_projects_the_month() {
        let conn = fixture();
        seed(&conn);
        let burn = rolling_burn(&conn, 5, Some(NOON), 0, None).expect("burn");
        // Today is the 31st: 2.25 + 0.25. The 30th's 1.5 is a different day.
        assert!((burn.today_cost - 2.5).abs() < 1e-12);
        // MTD is all three rows.
        assert!((burn.month_to_date - 4.0).abs() < 1e-12);
        // 31 July: days_so_far 31, days_left 0, so the projection IS the MTD.
        // `projected == month_to_date` is exactly what the SSE probe recorded
        // off the live server on the 31st, for exactly this reason.
        assert!((burn.projected_month_end - 4.0).abs() < 1e-12);
        assert_eq!(burn.projected_month_end, burn.month_to_date);

        // Mid-month the projection extrapolates instead: on the 15th, the same
        // $4 MTD averages 4/15 a day with 16 days left. (Both cutoffs are
        // *lower* bounds with no upper bound, so the 30th/31st rows — in the
        // future relative to this clock — land in BOTH buckets. That is
        // faithful: `_today_month_cost` only ever asks `ts >= ?`.)
        let mid = 1_784_116_800_000_000;
        assert_eq!(pytime::isoformat_utc(mid), "2026-07-15T12:00:00+00:00");
        let projected = rolling_burn(&conn, 5, Some(mid), 0, None).expect("burn");
        assert!((projected.today_cost - 4.0).abs() < 1e-12);
        assert!((projected.month_to_date - 4.0).abs() < 1e-12);
        assert!((projected.projected_month_end - (4.0 + (4.0 / 15.0) * 16.0)).abs() < 1e-12);
        assert!(projected.projected_month_end > projected.month_to_date);

        // …and the LOCAL calendar decides `days_left`, not the UTC one. At
        // 2026-07-31T12:00 UTC with a +720 offset the local date is already
        // 1 August, so the projection restarts on a 31-day month with 30 left.
        let next_month = rolling_burn(&conn, 5, Some(NOON), 720, None).expect("burn");
        assert_eq!(next_month.month_to_date, 0.0);
        assert_eq!(next_month.projected_month_end, 0.0);
    }

    #[test]
    fn the_window_sum_is_live_and_the_per_minute_divides_by_the_window() {
        let conn = fixture();
        seed(&conn);
        // 12:00 with a 5-minute window misses the 11:59 row by a minute? No —
        // 11:59 is inside 11:55..12:00, so it counts.
        let burn = rolling_burn(&conn, 5, Some(NOON), 0, None).expect("burn");
        assert!((burn.window_cost - 0.25).abs() < 1e-12);
        assert!((burn.per_minute - 0.25 / 5.0).abs() < 1e-15);
        assert!((burn.per_hour - (0.25 / 5.0) * 60.0).abs() < 1e-15);
        // A wider window pulls in the 09:00 row too.
        let wide = rolling_burn(&conn, 240, Some(NOON), 0, None).expect("burn");
        assert!((wide.window_cost - 2.5).abs() < 1e-12);
        // `max(window_minutes, 1)` — a zero window divides by one, not by zero.
        let zero = rolling_burn(&conn, 0, Some(NOON), 0, None).expect("burn");
        assert_eq!(zero.per_minute, zero.window_cost);
    }

    #[test]
    fn the_today_month_memo_is_keyed_on_the_cutoffs_and_expires() {
        let conn = fixture();
        seed(&conn);
        let cut = burn_cutoffs(NOON, 5, 0).expect("in range");
        let mut cache = BurnCache::default();
        let first = today_month_cost(&conn, cut.today_us, cut.month_us, NOON, Some(&mut cache))
            .expect("read");

        // A write the memo must NOT see while it is warm.
        conn.execute(
            "INSERT INTO usage_events (id, project_id, session_id, ts, day, model, cost_usd) \
             VALUES (99, 1, 's-one', '2026-07-31T11:00:00+00:00', '2026-07-31', 'opus', 100.0)",
            [],
        )
        .expect("insert");
        let warm = today_month_cost(
            &conn,
            cut.today_us,
            cut.month_us,
            NOON + 29_000_000,
            Some(&mut cache),
        )
        .expect("read");
        assert_eq!(warm, first);

        // Past the TTL it re-reads.
        let cold = today_month_cost(
            &conn,
            cut.today_us,
            cut.month_us,
            NOON + 31_000_000,
            Some(&mut cache),
        )
        .expect("read");
        assert!((cold.0 - (first.0 + 100.0)).abs() < 1e-12);

        // A clock step BACKWARDS makes `age` negative, which fails `0 <= age`
        // and forces a re-read rather than trusting a future-stamped entry.
        let mut back = BurnCache::default();
        today_month_cost(&conn, cut.today_us, cut.month_us, NOON, Some(&mut back)).expect("read");
        let stepped = today_month_cost(
            &conn,
            cut.today_us,
            cut.month_us,
            NOON - 5_000_000,
            Some(&mut back),
        )
        .expect("read");
        assert!((stepped.0 - cold.0).abs() < 1e-12);

        // And a different cutoff pair (local midnight moved) misses outright.
        let other = burn_cutoffs(NOON, 5, -480).expect("in range");
        let moved = today_month_cost(
            &conn,
            other.today_us,
            other.month_us,
            NOON,
            Some(&mut cache),
        )
        .expect("read");
        assert!(moved.0 <= cold.0);
    }

    // ── percentiles ─────────────────────────────────────────────────────────

    #[test]
    fn the_percentile_is_the_code_not_the_docstring() {
        // `int((p / 100.0) * N)`, clamped — NOT `ceil(p/100 * N) - 1`, which
        // the Python docstring claims and which would answer 58 below.
        let values: Vec<f64> = (0..61).map(f64::from).collect();
        // The whole point, spelled out: the product is a HAIR under 58, so a
        // truncation gives 57 and any rounding-up spelling gives 58.
        #[allow(clippy::cast_precision_loss, reason = "61")]
        let product = (95.0_f64 / 100.0) * values.len() as f64;
        assert_eq!(format!("{product:?}"), "57.949999999999996");
        assert_eq!(percentile(&values, 95.0), 57.0);
        assert_eq!(percentile(&values, 50.0), 30.0);
        // p99 of 61 is index 60, which is also the clamp.
        assert_eq!(percentile(&values, 99.0), 60.0);
        // p100 would index N; the clamp catches it.
        assert_eq!(percentile(&values, 100.0), 60.0);
    }

    #[test]
    fn the_percentile_short_circuits_on_zero_and_one_samples() {
        assert_eq!(percentile(&[], 50.0), 0.0);
        // A single sample returns itself for EVERY percentile, without the
        // index arithmetic — the explicit `len == 1` arm.
        assert_eq!(percentile(&[7.5], 99.0), 7.5);
        assert_eq!(percentile(&[7.5], 0.0), 7.5);
    }

    #[test]
    fn the_latency_table_is_sorted_by_sample_count_and_capped() {
        let conn = fixture();
        seed(&conn);
        // A window wide enough to catch the June rows too.
        let rows = tool_latency_percentiles(&conn, 24 * 40, 6, NOON).expect("latency");
        assert_eq!(rows.len(), 2);
        // Bash has two samples, Read one, so Bash sorts first.
        assert_eq!(rows[0].tool_name, "Bash");
        assert_eq!(rows[0].samples, 2);
        // Deltas: message 10 -> 11 is 2s, message 12 -> 13 is 10s.
        assert!((rows[0].p50 - 10.0).abs() < 1e-12);
        assert_eq!(rows[1].tool_name, "Read");
        assert_eq!(rows[1].samples, 1);
        assert!((rows[1].p50 - 1.0).abs() < 1e-12);

        // `out[: max(0, int(top_n))]` — a negative cap is an empty list, not a
        // panic and not the whole list.
        assert!(
            tool_latency_percentiles(&conn, 24 * 40, -1, NOON)
                .expect("latency")
                .is_empty()
        );
        assert_eq!(
            tool_latency_percentiles(&conn, 24 * 40, 1, NOON)
                .expect("latency")
                .len(),
            1
        );
    }

    #[test]
    fn a_cold_window_skips_the_lead_statement_entirely() {
        let conn = fixture();
        seed(&conn);
        // A one-hour window at noon on the 31st has no in-window mart rows.
        assert!(
            tool_latency_percentiles(&conn, 1, 6, NOON)
                .expect("latency")
                .is_empty()
        );
        // …and a store with no mart at all is the other early return.
        let bare = Connection::open_in_memory().expect("empty");
        assert!(
            tool_latency_percentiles(&bare, 24, 6, NOON)
                .expect("latency")
                .is_empty()
        );
    }

    #[test]
    fn a_mart_row_whose_source_message_is_gone_contributes_nothing() {
        let conn = fixture();
        seed(&conn);
        conn.execute(
            "INSERT INTO message_tool_mart \
             (id, message_id, project_id, session_id, ts, day, tool_name, file_path, byte_count, call_index) \
             VALUES (4, 999, 1, 's-one', '2026-07-30T10:00:00+00:00', '2026-07-30', 'Ghost', NULL, NULL, 0)",
            [],
        )
        .expect("insert");
        let rows = tool_latency_percentiles(&conn, 24 * 40, 6, NOON).expect("latency");
        assert!(rows.iter().all(|row| row.tool_name != "Ghost"));
    }

    #[test]
    fn a_last_message_in_a_session_has_no_next_and_is_dropped() {
        let conn = fixture();
        seed(&conn);
        // Message 13 is the last row in s-one, so LEAD gives NULL and the mart
        // row pointing at it must not produce a zero-second sample.
        conn.execute(
            "INSERT INTO message_tool_mart \
             (id, message_id, project_id, session_id, ts, day, tool_name, file_path, byte_count, call_index) \
             VALUES (5, 13, 1, 's-one', '2026-07-30T10:00:13+00:00', '2026-07-30', 'Tail', NULL, NULL, 0)",
            [],
        )
        .expect("insert");
        let rows = tool_latency_percentiles(&conn, 24 * 40, 6, NOON).expect("latency");
        assert!(rows.iter().all(|row| row.tool_name != "Tail"));
    }

    #[test]
    fn a_negative_delta_is_dropped_rather_than_poisoning_the_percentile() {
        let conn = fixture();
        seed(&conn);
        // Clock skew: seq 1 lands BEFORE seq 0 in wall-clock terms.
        conn.execute_batch(
            "INSERT INTO sessions VALUES (3, 1, 's-skew');
             INSERT INTO messages_202607 VALUES
               (30, 3, 0, '2026-07-30T10:00:05+00:00', 'assistant'),
               (31, 3, 1, '2026-07-30T10:00:00+00:00', 'user');
             INSERT INTO message_tool_mart
               (id, message_id, project_id, session_id, ts, day, tool_name,
                file_path, byte_count, call_index) VALUES
               (6, 30, 1, 's-skew', '2026-07-30T10:00:05+00:00', '2026-07-30', 'Skew', NULL, NULL, 0);",
        )
        .expect("insert");
        let rows = tool_latency_percentiles(&conn, 24 * 40, 6, NOON).expect("latency");
        assert!(rows.iter().all(|row| row.tool_name != "Skew"));
    }

    // ── readers and watermarks ──────────────────────────────────────────────

    #[test]
    fn the_watermarks_are_zero_on_a_missing_or_empty_table() {
        let bare = Connection::open_in_memory().expect("empty");
        assert_eq!(max_event_id(&bare).expect("read"), 0);
        assert_eq!(max_tool_call_id(&bare).expect("read"), 0);
        let conn = fixture();
        assert_eq!(max_event_id(&conn).expect("read"), 0);
        seed(&conn);
        assert_eq!(max_event_id(&conn).expect("read"), 3);
        assert_eq!(max_tool_call_id(&conn).expect("read"), 3);
    }

    #[test]
    fn the_incremental_readers_take_the_newest_page_and_return_it_ascending() {
        let conn = fixture();
        seed(&conn);
        let rows = recent_events(&conn, 0, 2).expect("read");
        assert_eq!(rows.len(), 2);
        // `ORDER BY id DESC LIMIT 2` picks {3, 2}; the reverse makes it 2 then 3.
        assert_eq!(rows[0]["id"], Value::from(2));
        assert_eq!(rows[1]["id"], Value::from(3));
        // Row 1 is DELIBERATELY skipped — the tail contract, not a bug.
        assert!(rows.iter().all(|row| row["id"] != 1));

        // Column order is the SELECT's, which is what the SSE payload renders.
        let keys: Vec<&str> = rows[0].keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            vec![
                "id",
                "ts",
                "project_id",
                "session_id",
                "model",
                "cost_usd",
                "input_tokens",
                "output_tokens",
                "cache_read_tokens",
                "cache_create_tokens",
                "cost_source",
                "project_slug",
                "project_name"
            ]
        );
        // The LEFT JOIN fills the project columns.
        assert_eq!(rows[0]["project_slug"], Value::from("demo"));

        let tools = recent_tool_calls(&conn, 1, 50).expect("read");
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0]["id"], Value::from(2));
        // A NULL column is `null`, not an empty string.
        assert_eq!(tools[0]["file_path"], Value::Null);
        assert_eq!(
            tools[1].keys().map(String::as_str).collect::<Vec<_>>(),
            vec![
                "id",
                "ts",
                "project_id",
                "session_id",
                "tool_name",
                "file_path",
                "byte_count",
                "call_index",
                "project_slug",
                "project_name"
            ]
        );

        let bare = Connection::open_in_memory().expect("empty");
        assert!(recent_events(&bare, 0, 50).expect("read").is_empty());
        assert!(recent_tool_calls(&bare, 0, 50).expect("read").is_empty());
    }

    // ── iso parsing ─────────────────────────────────────────────────────────

    #[test]
    fn the_iso_parser_only_rewrites_a_trailing_z_and_makes_naive_values_aware() {
        let zulu = iso_to_dt("2026-07-31T12:00:00Z").expect("trailing Z");
        let explicit = iso_to_dt("2026-07-31T12:00:00+00:00").expect("explicit offset");
        assert_eq!(zulu, explicit);

        // A naive value gets `tzinfo=UTC` WITHOUT moving the wall clock, which
        // is what makes the subtraction below legal instead of a `TypeError`.
        let naive = iso_to_dt("2026-07-31T12:00:00").expect("naive");
        assert_eq!(naive.offset_s, Some(0));
        assert_eq!(naive.sub_total_seconds(explicit), Some(0.0));

        // An interior `Z` is NOT rewritten, so `fromisoformat` raises.
        assert!(iso_to_dt("2026-07-31TZ12:00:00").is_none());
        assert!(iso_to_dt("").is_none());
        assert!(iso_to_dt("not a timestamp").is_none());

        // A real offset participates in the subtraction.
        let west = iso_to_dt("2026-07-31T04:00:00-08:00").expect("offset");
        assert_eq!(west.sub_total_seconds(explicit), Some(0.0));
    }

    // ── the snapshot envelope ───────────────────────────────────────────────

    #[test]
    fn the_snapshot_is_three_keys_in_the_literals_order() {
        let conn = fixture();
        seed(&conn);
        let snap = snapshot(&conn, 5, 24, 6, 0).expect("snapshot");
        let obj = snap.as_object().expect("object");
        assert_eq!(
            obj.keys().map(String::as_str).collect::<Vec<_>>(),
            vec!["burn", "tool_latency", "watermarks"]
        );
        let burn = obj["burn"].as_object().expect("burn");
        assert_eq!(
            burn.keys().map(String::as_str).collect::<Vec<_>>(),
            vec![
                "window_minutes",
                "window_cost",
                "per_minute",
                "per_hour",
                "today_cost",
                "month_to_date",
                "projected_month_end",
                "ts"
            ]
        );
        // `window_minutes` is an INT in the payload; every other burn field is
        // a float, and `0.0` must render with its decimal point.
        assert!(burn["window_minutes"].is_i64());
        assert!(burn["window_cost"].is_f64());
        assert_eq!(
            obj["watermarks"]
                .as_object()
                .expect("watermarks")
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            vec!["event_id", "tool_call_id"]
        );
        // An out-of-range offset propagates the raise all the way up.
        assert!(matches!(
            snapshot(&conn, 5, 24, 6, i64::MIN),
            Err(LiveError::DateOverflow)
        ));
    }
}
