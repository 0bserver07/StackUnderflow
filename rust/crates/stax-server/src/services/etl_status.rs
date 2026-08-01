//! `etl/status.py` (566 ln) — the ETL health snapshot behind
//! `GET /api/etl/status`.
//!
//! | Item | Python | Notes |
//! |---|---|---|
//! | [`assemble_status`] | `assemble_status` | the whole payload |
//! | [`events_summary`] | `_events_summary` | one `GROUP BY provider, cost_source` |
//! | [`marts_summary`] | `_marts_summary` | five names, always all five |
//! | [`coverage_summary`] | `_coverage_summary` | the anti-join that found 91/334 |
//! | [`watcher_state`] | `_watcher_state` | env + handle + lock file |
//! | [`read_lock_holder`] | `etl/lock.py::read_lock_holder` | the PID probe |
//! | [`compute_lag`] | `_compute_lag` | `max_id - min(watermark)` |
//! | [`compute_health`] | `_compute_health` | four states, worst-first |
//!
//! The route is four lines; everything below is why this file exists. Python
//! keeps the SQL in the assembler so `stackunderflow etl status` and the HTTP
//! surface cannot disagree, and the same split is reproduced here so wave 8's
//! CLI verb has one home to call.
//!
//! # What is load-bearing
//!
//! * **`lag_seconds` is not seconds.** It is `max(usage_events.id) - min(mart
//!   watermark)` — a count of lagging *events*. The module docstring calls the
//!   name a spec misnomer and keeps it because renaming would break the route
//!   contract. Ported name-for-name; do not "fix" it.
//! * **`_table_exists` here is `type='table'`** — the DIV-148 asymmetry. Every
//!   object it probes (`usage_events`, `mart_watermark`, the five `*_mart`
//!   tables, `projects`, `project_mart`) is a real table; the partitioned
//!   `messages` VIEW is never probed by this module, so `type='table'` is the
//!   correct guard and [`crate::services::mart_queries::table_exists`] is the
//!   deduped owner of it (law 9). A `type IN ('table','view')` probe here would
//!   be a *different* function that happens to agree on this store.
//! * **The rollups are `+=`, not `sum()`.** `total += n`, `by_provider[p] =
//!   by_provider.get(p, 0) + n`. Integer arithmetic, so Neumaier compensation is
//!   not merely unnecessary, it is the wrong operation (law 3). `total` counts
//!   every group *before* the truthiness filters, so a row with a blank provider
//!   raises `total` and appears in no breakdown.
//! * **Breakdown key order is the `GROUP BY`'s row order**, because Python
//!   inserts into a `dict` as the cursor yields. `serde_json`'s `preserve_order`
//!   feature makes the same first-seen order reach the wire.
//! * **Every block degrades to zeros on a missing table.** A fresh pre-Wave-1
//!   store must answer, not 500.
//!
//! # The wall clock is NOT in this payload — with two exits
//!
//! A status endpoint is where a `scanned_at` or a "seconds since" normally
//! lives, and the DIV-073 / DIV-085 class of permanently-open case rows is what
//! that costs. This one is almost clean:
//!
//! * `marts[*].last_refresh_ts` is read from `mart_watermark`, so it is whatever
//!   the last writer stored — a stored string, not a reading.
//! * `lag_seconds` is an event count.
//! * `watcher.seconds_since_refresh` **is** a `datetime.now(UTC)` subtraction —
//!   but it is computed only when the watcher handle exposes a
//!   `last_refresh_ts`, and `_watcher_state`'s own comment records that Wave 2C
//!   exposes neither field. It is `None` on every reachable path today.
//! * `current_job.started_at` / `last_job.completed_at` are wall-clock stamps,
//!   but both slots are `None` unless a backfill was triggered **in this
//!   process** within the last thirty seconds.
//!
//! So the body is deterministic for a differ. What is *not* deterministic is
//! `watcher.enabled`, and not because of a clock — see the numbered finding in
//! `parity/DIV-e-etl.md` about `parity/pyserver.py` setting
//! `STACKUNDERFLOW_DISABLE_WATCHER=1` in the Python interpreter only.

use std::path::Path;

use rusqlite::Connection;
use serde_json::{Map, Value};

use crate::services::etl_backfill::{get_current_job, get_last_job};
use crate::services::mart_queries::table_exists;

/// `STALE_LAG_THRESHOLD_EVENTS` — above this a mart is "stale", not "syncing".
pub const STALE_LAG_THRESHOLD_EVENTS: i64 = 100;

/// `SYNCING_RECENT_SECONDS` — a refresh this recent keeps a lagging store
/// "syncing" rather than letting it drift.
pub const SYNCING_RECENT_SECONDS: i64 = 10;

/// `KNOWN_MART_NAMES` — the five marts the surface always renders.
///
/// Note it is FIVE, while `stax_etl::marts::all()` registers EIGHT. `tool`,
/// `command` and `message_tool` are refreshed by a backfill and are invisible
/// here, so their watermarks never enter [`compute_lag`]. Reproduced as written;
/// widening the list would change `lag_seconds` and `health` on any store where
/// those three trail.
pub const KNOWN_MART_NAMES: [&str; 5] =
    ["daily", "session", "project", "provider_day", "model_day"];

/// `COVERAGE_SAMPLE_LIMIT` — uncovered project ids carried in the payload.
pub const COVERAGE_SAMPLE_LIMIT: i64 = 20;

/// `_MART_TABLES` — mart name → the table its `row_count` comes from.
fn mart_table(name: &str) -> &'static str {
    match name {
        "daily" => "daily_mart",
        "session" => "session_mart",
        "project" => "project_mart",
        "provider_day" => "provider_day_mart",
        _ => "model_day_mart",
    }
}

// ── public entry point ───────────────────────────────────────────────────────

/// `assemble_status(conn)` — the full ETL status payload.
///
/// `app_dir` is `settings.app_dir()`, the directory `etl/lock.py` derives
/// `server.lock` from; `watcher_disable_env` is the raw
/// `STACKUNDERFLOW_DISABLE_WATCHER` value (`None` when unset) and `now_micros`
/// is `datetime.now(UTC)` — all three injected rather than read here, because
/// the workspace forbids `unsafe` and therefore forbids a test that mutates the
/// environment (ARCHITECT-STATE finding 5).
///
/// Key order is the literal's: `watcher`, `marts`, `events`, `coverage`,
/// `lag_seconds`, `health`, `current_job`, `last_job` — **not** the order the
/// blocks are computed in, which is events-first.
///
/// # Errors
/// Any SQLite error. Python has no `except` here either: a broken store is a
/// 500, not a payload of zeros. The zeros are for a *missing table*, which is a
/// different thing and is handled per block.
pub fn assemble_status(
    conn: &Connection,
    app_dir: &Path,
    watcher_disable_env: Option<&str>,
    now_micros: i64,
) -> rusqlite::Result<Value> {
    let events = events_summary(conn)?;
    let marts = marts_summary(conn)?;
    let coverage = coverage_summary(conn)?;
    let watcher = watcher_state(app_dir, watcher_disable_env);
    let lag = compute_lag(events.max_id, &marts.watermarks);
    let mut health = compute_health(events.max_id, &watcher, lag);

    // Both slots can change between back-to-back calls with no DB activity.
    let current = get_current_job();
    let last = get_last_job(now_micros);

    // Escalate to `error` while a recently failed backfill is inside the TTL
    // window. Deliberately AFTER `compute_health`, and unconditional: a live
    // pipeline with a fresh failure still reports `error` for those 30 seconds.
    if last.as_ref().is_some_and(|job| job.status == "failed") {
        health = "error";
    }

    let mut out = Map::new();
    out.insert("watcher".to_owned(), watcher);
    out.insert("marts".to_owned(), marts.value);
    out.insert("events".to_owned(), events.value);
    out.insert("coverage".to_owned(), coverage);
    out.insert("lag_seconds".to_owned(), Value::from(lag));
    out.insert("health".to_owned(), Value::from(health));
    out.insert(
        "current_job".to_owned(),
        current.map_or(Value::Null, |job| job.current_value()),
    );
    out.insert(
        "last_job".to_owned(),
        last.map_or(Value::Null, |job| job.last_value()),
    );
    Ok(Value::Object(out))
}

// ── events ───────────────────────────────────────────────────────────────────

/// The `events` block plus the `max_id` [`compute_lag`] needs.
#[derive(Debug)]
pub struct EventsSummary {
    /// `{total, max_id, by_provider, by_cost_source}`.
    pub value: Value,
    /// `COALESCE(MAX(id), 0)`.
    pub max_id: i64,
}

/// `_events_summary` — one `GROUP BY provider, cost_source` pass, summed host
/// side.
///
/// The two breakdowns used to be two `GROUP BY` passes and `GROUP BY
/// cost_source` had no index to ride, so it was a full table scan on every 10 s
/// poll (#43). One pass yields a tiny cross-tab; `total` falls out of it, which
/// is why the standalone `COUNT(*)` is gone. Reproduce the fold, not the three
/// scans it replaced.
///
/// # Errors
/// Any SQLite error.
pub fn events_summary(conn: &Connection) -> rusqlite::Result<EventsSummary> {
    if !table_exists(conn, "usage_events")? {
        let mut out = Map::new();
        out.insert("total".to_owned(), Value::from(0));
        out.insert("max_id".to_owned(), Value::from(0));
        out.insert("by_provider".to_owned(), Value::Object(Map::new()));
        out.insert("by_cost_source".to_owned(), Value::Object(Map::new()));
        return Ok(EventsSummary {
            value: Value::Object(out),
            max_id: 0,
        });
    }

    let max_id: i64 = conn.query_row(
        "SELECT COALESCE(MAX(id), 0) AS m FROM usage_events",
        [],
        |r| r.get(0),
    )?;

    let mut total: i64 = 0;
    let mut by_provider = Map::new();
    let mut by_cost_source = Map::new();
    let mut stmt = conn.prepare(
        "SELECT provider, cost_source, COUNT(*) AS n \
         FROM usage_events GROUP BY provider, cost_source",
    )?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        // NULL and non-TEXT are both possible on a store an older writer touched;
        // `r["provider"]` in Python hands back whatever the driver produced and
        // `if prov:` then tests Python truthiness. `None` and `""` are both
        // falsy, and so is the integer `0` — the three collapse to "no key" here.
        let provider = truthy_text(row.get_ref(0)?);
        let cost_source = truthy_text(row.get_ref(1)?);
        let n: i64 = row.get(2)?;

        // `total` counts every event, including rows whose provider or
        // cost_source is blank — summed BEFORE the truthiness filters so it stays
        // identical to the `COUNT(*)` it replaced.
        total += n;
        if let Some(key) = provider {
            bump(&mut by_provider, &key, n);
        }
        if let Some(key) = cost_source {
            bump(&mut by_cost_source, &key, n);
        }
    }

    let mut out = Map::new();
    out.insert("total".to_owned(), Value::from(total));
    out.insert("max_id".to_owned(), Value::from(max_id));
    out.insert("by_provider".to_owned(), Value::Object(by_provider));
    out.insert("by_cost_source".to_owned(), Value::Object(by_cost_source));
    Ok(EventsSummary {
        value: Value::Object(out),
        max_id,
    })
}

/// `d[key] = d.get(key, 0) + n` — a `+=` chain over integers (law 3).
fn bump(map: &mut Map<String, Value>, key: &str, n: i64) {
    let previous = map.get(key).and_then(Value::as_i64).unwrap_or(0);
    map.insert(key.to_owned(), Value::from(previous + n));
}

/// `if prov:` — the value as a dict key, or `None` when Python would call it
/// falsy.
fn truthy_text(value: rusqlite::types::ValueRef<'_>) -> Option<String> {
    use rusqlite::types::ValueRef;
    match value {
        ValueRef::Null => None,
        ValueRef::Text(bytes) => {
            let text = String::from_utf8_lossy(bytes).into_owned();
            (!text.is_empty()).then_some(text)
        }
        // A numeric provider is not reachable through any writer, but `if prov:`
        // would accept a non-zero one and `dict[int]` would key on it. JSON has
        // no numeric keys, so `json.dumps` stringifies it on the way out —
        // `dumps_http` of the bare scalar is that same rendering, CPython float
        // repr included, and it is the deduped owner of it (law 9).
        ValueRef::Integer(n) => (n != 0).then(|| n.to_string()),
        ValueRef::Real(x) => (x != 0.0).then(|| stax_memory::pyjson::dumps_http(&Value::from(x))),
        ValueRef::Blob(bytes) => {
            (!bytes.is_empty()).then(|| String::from_utf8_lossy(bytes).into_owned())
        }
    }
}

// ── marts ────────────────────────────────────────────────────────────────────

/// The `marts` block plus the watermarks [`compute_lag`] folds.
#[derive(Debug)]
pub struct MartsSummary {
    /// `{mart_name: {watermark, row_count, last_refresh_ts}}`.
    pub value: Value,
    /// One entry per [`KNOWN_MART_NAMES`] member, in that order.
    pub watermarks: Vec<i64>,
}

/// `_marts_summary` — all five names, always, zeros where the store has nothing.
///
/// One round trip pulls every `mart_watermark` row (including the three the
/// surface does not render, which are then ignored); the per-mart `COUNT(*)` is
/// separate and guarded by its own table probe.
///
/// # Errors
/// Any SQLite error.
pub fn marts_summary(conn: &Connection) -> rusqlite::Result<MartsSummary> {
    let mut watermarks: Vec<(String, i64, Value)> = Vec::new();
    if table_exists(conn, "mart_watermark")? {
        let mut stmt =
            conn.prepare("SELECT mart_name, last_event_id, last_refresh_ts FROM mart_watermark")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let name: String = row.get(0)?;
            let last_event_id: i64 = row.get(1)?;
            let ts: Option<String> = row.get(2)?;
            watermarks.push((name, last_event_id, ts.map_or(Value::Null, Value::from)));
        }
    }

    let mut out = Map::new();
    let mut folded = Vec::with_capacity(KNOWN_MART_NAMES.len());
    for name in KNOWN_MART_NAMES {
        // `watermarks.get(name, (0, None))` — a dict, so a duplicated
        // `mart_name` would be last-wins. `mart_name` is the PRIMARY KEY, so
        // duplicates cannot arise; `.rev().find()` spells last-wins anyway
        // rather than relying on that.
        let (wm, ts) = watermarks
            .iter()
            .rev()
            .find(|(key, _, _)| key == name)
            .map_or((0, Value::Null), |(_, wm, ts)| (*wm, ts.clone()));

        let table = mart_table(name);
        let row_count: i64 = if table_exists(conn, table)? {
            conn.query_row(&format!("SELECT COUNT(*) AS n FROM {table}"), [], |r| {
                r.get(0)
            })?
        } else {
            0
        };

        let mut block = Map::new();
        block.insert("watermark".to_owned(), Value::from(wm));
        block.insert("row_count".to_owned(), Value::from(row_count));
        block.insert("last_refresh_ts".to_owned(), ts);
        out.insert((*name).to_owned(), Value::Object(block));
        folded.push(wm);
    }

    Ok(MartsSummary {
        value: Value::Object(out),
        watermarks: folded,
    })
}

// ── coverage ─────────────────────────────────────────────────────────────────

/// `_coverage_summary` — how many `projects` rows have no `project_mart` row.
///
/// The counter that made a 91-of-334 gap observable. Lag only compares
/// watermarks, so a project that never produced a `usage_event` is not "behind"
/// — it is absent, and every mart-backed read path silently omits it.
///
/// Three branches, all reproduced: no `projects` table (all zeros), no
/// `project_mart` table (everything uncovered, sample from `projects` directly),
/// and the anti-join. Note the last one issues its sample query **only when
/// `missing`** is non-zero.
///
/// # Errors
/// Any SQLite error.
pub fn coverage_summary(conn: &Connection) -> rusqlite::Result<Value> {
    let block = |projects: i64, with_mart: i64, without: i64, sample: Vec<Value>| {
        let mut out = Map::new();
        out.insert("projects".to_owned(), Value::from(projects));
        out.insert("projects_with_mart".to_owned(), Value::from(with_mart));
        out.insert("projects_without_mart".to_owned(), Value::from(without));
        out.insert(
            "projects_without_mart_sample".to_owned(),
            Value::Array(sample),
        );
        Value::Object(out)
    };

    if !table_exists(conn, "projects")? {
        return Ok(block(0, 0, 0, Vec::new()));
    }

    if !table_exists(conn, "project_mart")? {
        // No mart table at all: every project is uncovered. Note the ORDER OF
        // STATEMENTS — the sample is fetched first and the total second, which
        // is invisible on the wire but is what the reference does.
        let mut stmt = conn.prepare("SELECT id FROM projects ORDER BY id LIMIT ?")?;
        let sample: Vec<Value> = stmt
            .query_map([COVERAGE_SAMPLE_LIMIT], |row| row.get::<_, i64>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?
            .into_iter()
            .map(Value::from)
            .collect();
        let total: i64 =
            conn.query_row("SELECT COUNT(*) AS n FROM projects", [], |row| row.get(0))?;
        return Ok(block(total, 0, total, sample));
    }

    let (total, covered): (i64, i64) = conn.query_row(
        "SELECT COUNT(*) AS total, COUNT(m.project_id) AS covered \
         FROM projects p LEFT JOIN project_mart m ON m.project_id = p.id",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let missing = (total - covered).max(0);

    let mut sample: Vec<Value> = Vec::new();
    if missing != 0 {
        let mut stmt = conn.prepare(
            "SELECT p.id AS id FROM projects p \
             LEFT JOIN project_mart m ON m.project_id = p.id \
             WHERE m.project_id IS NULL ORDER BY p.id LIMIT ?",
        )?;
        sample = stmt
            .query_map([COVERAGE_SAMPLE_LIMIT], |row| row.get::<_, i64>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?
            .into_iter()
            .map(Value::from)
            .collect();
    }

    Ok(block(total, covered, missing, sample))
}

// ── watcher ──────────────────────────────────────────────────────────────────

/// `_watcher_state` — a best-effort snapshot of the Wave 2C watcher.
///
/// **`running` is always the string `"unknown"` here, and that is the faithful
/// answer, not a stub.** Python reads `deps.watcher_handle`, which the FastAPI
/// lifespan sets *iff* the watcher actually started; the whole `handle is None`
/// branch exists because CLI callers and `--no-watcher` servers hit it. The Rust
/// port has no watcher thread at all (`stax-server` never spawns one), so the
/// handle is unconditionally absent and the `None` branch is unconditionally
/// taken. `last_refresh_ts`, `seconds_since_refresh` and `events_in_last_cycle`
/// are `None` for the same reason — and Python's own comment records that Wave
/// 2C exposes none of the three on the handle either, so the non-`None` branch
/// produces the identical three nulls today.
///
/// `_seconds_since` is therefore **not ported**: it is reachable only from a
/// handle field that no code path sets on either side. Porting an unreachable
/// clock subtraction into a status payload would be the DIV-073 hazard, invited.
///
/// `enabled` is the one field that is genuinely computed, and it is the one that
/// diverges under the shared harness — see the module docs and the ledger.
#[must_use]
pub fn watcher_state(app_dir: &Path, watcher_disable_env: Option<&str>) -> Value {
    let mut out = Map::new();
    out.insert(
        "enabled".to_owned(),
        Value::Bool(!watcher_env_disabled(watcher_disable_env)),
    );
    out.insert("running".to_owned(), Value::from("unknown"));
    out.insert("last_refresh_ts".to_owned(), Value::Null);
    out.insert("seconds_since_refresh".to_owned(), Value::Null);
    out.insert("events_in_last_cycle".to_owned(), Value::Null);
    out.insert(
        "lock_held_by".to_owned(),
        read_lock_holder(app_dir).map_or(Value::Null, Value::from),
    );
    Value::Object(out)
}

/// `_watcher_env_disabled` — `val.strip().lower() in ("1","true","yes","on")`.
///
/// `str.strip()` with no argument strips Unicode whitespace and `str.lower()`
/// does full Unicode case folding; `trim` / `to_lowercase` here are the ASCII-
/// equivalent for every value that could reach this (the four accepted spellings
/// are ASCII). A narrowing on exotic input, and named as one.
#[must_use]
pub fn watcher_env_disabled(raw: Option<&str>) -> bool {
    watcher_env_disabled_impl(raw)
}

fn watcher_env_disabled_impl(raw: Option<&str>) -> bool {
    let value = raw.unwrap_or_default().trim().to_lowercase();
    matches!(value.as_str(), "1" | "true" | "yes" | "on")
}

/// `etl/lock.py::read_lock_holder` — the PID recorded in `<app_dir>/server.lock`.
///
/// Advisory and informational: the OS-level `flock` is the real gate, and this
/// value is only rendered so an operator can see which instance owns the
/// watcher. Every failure is `None` — a missing file, an unreadable one, an
/// empty one, a first line that is not an integer. Python additionally wraps the
/// whole call in `_read_lock_holder_safe`'s bare `except Exception`, which is
/// what turns a `UnicodeDecodeError` (NOT in `read_lock_holder`'s own `except`
/// tuple) into `None` rather than a 500; `from_utf8` failing is the same `None`
/// here.
///
/// Format is `<pid>\n<start_ts>`, and PID-only files from older versions are
/// accepted — hence "first line", not "whole file".
#[must_use]
pub fn read_lock_holder(app_dir: &Path) -> Option<i64> {
    let text = std::fs::read_to_string(app_dir.join("server.lock")).ok()?;
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    // `text.splitlines()[0].strip()`. `splitlines` also breaks on \v, \f, \x1c
    // and friends; `lines()` is \n / \r\n only. No writer emits those, and the
    // consequence of the difference is at worst a `None` where Python found a
    // PID — an informational field.
    py_int(text.lines().next().unwrap_or_default().trim())
}

/// `int(s)` for the ASCII forms a lock file can hold.
///
/// CPython accepts a leading sign, surrounding whitespace, and `_` digit
/// separators (never leading, trailing, or doubled). It also accepts non-ASCII
/// decimal digits, which no writer of this file produces.
fn py_int(text: &str) -> Option<i64> {
    let text = text.trim();
    let (sign, digits) = match text.strip_prefix('-') {
        Some(rest) => (-1_i64, rest),
        None => (1_i64, text.strip_prefix('+').unwrap_or(text)),
    };
    if digits.is_empty() || digits.starts_with('_') || digits.ends_with('_') {
        return None;
    }
    let mut cleaned = String::with_capacity(digits.len());
    let mut previous_underscore = false;
    for ch in digits.chars() {
        if ch == '_' {
            if previous_underscore {
                return None;
            }
            previous_underscore = true;
            continue;
        }
        if !ch.is_ascii_digit() {
            return None;
        }
        previous_underscore = false;
        cleaned.push(ch);
    }
    cleaned.parse::<i64>().ok().map(|n| sign * n)
}

// ── lag + health ─────────────────────────────────────────────────────────────

/// `_compute_lag` — `max(0, max_event_id - min(mart watermarks))`.
///
/// A registered mart whose watermark is `0` because it never refreshed counts as
/// a 0-watermark, not "unknown", which correctly lights up "stale" the moment
/// any events exist. `if not marts` cannot fire — [`marts_summary`] always
/// returns five — and `max_event_id == 0` short-circuits an empty store.
#[must_use]
pub fn compute_lag(max_event_id: i64, watermarks: &[i64]) -> i64 {
    if watermarks.is_empty() || max_event_id == 0 {
        return 0;
    }
    let min_wm = watermarks.iter().copied().min().unwrap_or(0);
    (max_event_id - min_wm).max(0)
}

/// `_compute_health` — `"live" | "syncing" | "stale" | "error"`, worst first.
///
/// * **error** — lag over threshold AND `watcher.running is False`;
/// * **stale** — lag over threshold;
/// * **syncing** — lag above zero and a refresh inside the last 10 s;
/// * **live** — everything else, including "lag above zero but no recent
///   refresh", which is deliberate: a single in-flight insert must not flicker
///   the badge.
///
/// The `error` branch is **unreachable in the port**: `running` is the string
/// `"unknown"` on every path (see [`watcher_state`]), and `"unknown" is False`
/// is `False` in Python too — so the branch is dead on the reference's own
/// `handle is None` path as well. It is written out because it is dead by
/// *value*, not by construction, and a future watcher would revive both sides
/// together.
#[must_use]
pub fn compute_health(max_event_id: i64, watcher: &Value, lag_events: i64) -> &'static str {
    if max_event_id == 0 {
        return "live";
    }
    if lag_events > STALE_LAG_THRESHOLD_EVENTS {
        if watcher.get("running") == Some(&Value::Bool(false)) {
            return "error";
        }
        return "stale";
    }
    if lag_events > 0 {
        // `if seconds_since is not None and seconds_since <= SYNCING_RECENT_SECONDS`.
        let seconds_since = watcher.get("seconds_since_refresh").and_then(Value::as_i64);
        if seconds_since.is_some_and(|s| s <= SYNCING_RECENT_SECONDS) {
            return "syncing";
        }
        return "live";
    }
    "live"
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::etl_backfill::{
        complete_job, reset_for_tests, start_job, test_lock, testdb,
    };

    const T0: i64 = 1_767_312_000_000_000;

    fn scratch_dir(tag: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("stax-etl-status-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    fn status(conn: &Connection) -> Value {
        assemble_status(conn, &scratch_dir("empty"), None, T0).expect("status")
    }

    /// A store with none of the ETL tables must answer a complete payload of
    /// zeros, not 500 and not a payload with keys missing.
    #[test]
    fn a_pre_wave_one_store_degrades_to_a_complete_payload_of_zeros() {
        let _guard = test_lock();
        reset_for_tests();
        let conn = Connection::open_in_memory().expect("store");
        let body = crate::json::JsonBody::ok(status(&conn)).render();
        assert_eq!(
            body,
            concat!(
                r#"{"watcher":{"enabled":true,"running":"unknown","last_refresh_ts":null,"#,
                r#""seconds_since_refresh":null,"events_in_last_cycle":null,"lock_held_by":null},"#,
                r#""marts":{"daily":{"watermark":0,"row_count":0,"last_refresh_ts":null},"#,
                r#""session":{"watermark":0,"row_count":0,"last_refresh_ts":null},"#,
                r#""project":{"watermark":0,"row_count":0,"last_refresh_ts":null},"#,
                r#""provider_day":{"watermark":0,"row_count":0,"last_refresh_ts":null},"#,
                r#""model_day":{"watermark":0,"row_count":0,"last_refresh_ts":null}},"#,
                r#""events":{"total":0,"max_id":0,"by_provider":{},"by_cost_source":{}},"#,
                r#""coverage":{"projects":0,"projects_with_mart":0,"projects_without_mart":0,"#,
                r#""projects_without_mart_sample":[]},"lag_seconds":0,"health":"live","#,
                r#""current_job":null,"last_job":null}"#
            )
        );
    }

    /// The five mart names are rendered in `KNOWN_MART_NAMES` order — the
    /// registry's, not the alphabet's, and not the eight `marts::all()` returns.
    #[test]
    fn the_mart_block_is_five_names_in_registration_order() {
        let _guard = test_lock();
        let conn = testdb::conn();
        let body = status(&conn);
        let names: Vec<&String> = body["marts"]
            .as_object()
            .expect("marts is an object")
            .keys()
            .collect();
        assert_eq!(
            names,
            vec!["daily", "session", "project", "provider_day", "model_day"]
        );
    }

    /// `total` counts rows a breakdown drops. Three events, one with a blank
    /// provider: `total` is 3, `by_provider` sums to 2.
    #[test]
    fn total_counts_the_rows_the_truthiness_filters_drop() {
        let _guard = test_lock();
        let conn = testdb::conn();
        testdb::event(&conn, 1, "claude", "rate_card");
        testdb::event(&conn, 2, "claude", "estimated");
        testdb::event(&conn, 3, "", "");
        let events = events_summary(&conn).expect("events");
        assert_eq!(events.value["total"], Value::from(3));
        assert_eq!(events.value["max_id"], Value::from(3));
        assert_eq!(events.value["by_provider"]["claude"], Value::from(2));
        assert_eq!(
            events.value["by_provider"].as_object().expect("obj").len(),
            1,
            "the blank provider is not a key"
        );
        assert_eq!(
            events.value["by_cost_source"]
                .as_object()
                .expect("obj")
                .len(),
            2
        );
    }

    /// The anti-join, its sample cap, and the "no sample when nothing is
    /// missing" short circuit.
    #[test]
    fn coverage_counts_and_samples_the_projects_no_mart_row_covers() {
        let _guard = test_lock();
        let conn = testdb::conn();
        for id in 1..=25 {
            conn.execute(
                "INSERT INTO projects (id, provider, slug, display_name)
                 VALUES (?, 'claude', 'p' || ?, 'p')",
                rusqlite::params![id, id],
            )
            .expect("project");
        }
        conn.execute(
            "INSERT INTO project_mart (project_id, provider, slug, display_name)
             VALUES (1, 'claude', 'p1', 'p')",
            [],
        )
        .expect("mart row");

        let coverage = coverage_summary(&conn).expect("coverage");
        assert_eq!(coverage["projects"], Value::from(25));
        assert_eq!(coverage["projects_with_mart"], Value::from(1));
        assert_eq!(coverage["projects_without_mart"], Value::from(24));
        let sample = coverage["projects_without_mart_sample"]
            .as_array()
            .expect("array");
        assert_eq!(sample.len(), 20, "COVERAGE_SAMPLE_LIMIT");
        assert_eq!(sample[0], Value::from(2), "id 1 is covered");

        // Cover the rest: the sample query is not issued at all.
        conn.execute(
            "INSERT INTO project_mart (project_id, provider, slug, display_name)
             SELECT id, 'claude', slug, 'p' FROM projects WHERE id > 1",
            [],
        )
        .expect("cover");
        let coverage = coverage_summary(&conn).expect("coverage");
        assert_eq!(coverage["projects_without_mart"], Value::from(0));
        assert_eq!(
            coverage["projects_without_mart_sample"],
            Value::Array(Vec::new())
        );
    }

    /// The four health states, driven through the real assembler where possible
    /// and through `compute_health` for the branch no reachable watcher can set.
    #[test]
    fn health_is_worst_first_over_the_lag_threshold() {
        let _guard = test_lock();
        let watcher = watcher_state(&scratch_dir("health"), None);
        // Empty store: live before anything else is even looked at.
        assert_eq!(compute_health(0, &watcher, 9_999), "live");
        // Over the threshold with no `running: false`: stale, not error.
        assert_eq!(compute_health(500, &watcher, 101), "stale");
        assert_eq!(compute_health(500, &watcher, 100), "live", "> not >=");
        // Lag but no recent refresh: still live — one in-flight insert must not
        // flicker the badge.
        assert_eq!(compute_health(500, &watcher, 1), "live");

        // The two branches only a watcher handle could reach.
        let mut down = watcher.clone();
        down["running"] = Value::Bool(false);
        assert_eq!(compute_health(500, &down, 101), "error");
        let mut syncing = watcher.clone();
        syncing["seconds_since_refresh"] = Value::from(10);
        assert_eq!(compute_health(500, &syncing, 1), "syncing");
        syncing["seconds_since_refresh"] = Value::from(11);
        assert_eq!(compute_health(500, &syncing, 1), "live");
    }

    /// `lag_seconds` is `max_id - min(watermark)` over the FIVE rendered marts,
    /// so a lagging `tool_mart` is invisible to it.
    #[test]
    fn lag_folds_the_five_rendered_marts_and_ignores_the_other_three() {
        let _guard = test_lock();
        let conn = testdb::conn();
        for id in 1..=300 {
            testdb::event(&conn, id, "claude", "rate_card");
        }
        for name in ["daily", "session", "project", "provider_day", "model_day"] {
            conn.execute(
                "INSERT INTO mart_watermark (mart_name, last_event_id, last_refresh_ts)
                 VALUES (?, 300, '2026-01-01T00:00:00+00:00')",
                [name],
            )
            .expect("watermark");
        }
        conn.execute(
            "INSERT INTO mart_watermark (mart_name, last_event_id, last_refresh_ts)
             VALUES ('tool', 1, '2026-01-01T00:00:00+00:00')",
            [],
        )
        .expect("lagging tool watermark");

        let body = status(&conn);
        assert_eq!(body["lag_seconds"], Value::from(0));
        assert_eq!(body["health"], Value::from("live"));
        assert!(body["marts"].get("tool").is_none(), "not a rendered mart");

        // Now drop one rendered mart's watermark: the lag reappears.
        conn.execute(
            "UPDATE mart_watermark SET last_event_id = 5 WHERE mart_name = 'model_day'",
            [],
        )
        .expect("regress");
        let body = status(&conn);
        assert_eq!(body["lag_seconds"], Value::from(295));
        assert_eq!(body["health"], Value::from("stale"));
    }

    /// The job slots reach the payload, and a failed one escalates `health` to
    /// `error` even on a perfectly caught-up pipeline.
    #[test]
    fn a_failed_backfill_inside_the_ttl_escalates_health_to_error() {
        let _guard = test_lock();
        reset_for_tests();
        let conn = testdb::conn();
        let dir = scratch_dir("jobs");

        let job = start_job(true, T0).expect("claim");
        let body = assemble_status(&conn, &dir, None, T0).expect("status");
        assert_eq!(body["current_job"]["status"], Value::from("running"));
        assert_eq!(body["current_job"]["force"], Value::Bool(true));
        assert_eq!(body["last_job"], Value::Null);
        assert_eq!(body["health"], Value::from("live"));

        complete_job(&job.job_id, "failed", Some("boom".to_owned()), T0 + 1_000);
        let body = assemble_status(&conn, &dir, None, T0 + 1_000).expect("status");
        assert_eq!(body["current_job"], Value::Null);
        assert_eq!(body["last_job"]["error"], Value::from("boom"));
        assert_eq!(
            body["health"],
            Value::from("error"),
            "an empty store is otherwise live"
        );

        // Past the TTL the escalation stops on its own, no sweeper involved.
        let body = assemble_status(&conn, &dir, None, T0 + 1_000 + 30_000_001).expect("status");
        assert_eq!(body["last_job"], Value::Null);
        assert_eq!(body["health"], Value::from("live"));
        reset_for_tests();
    }

    #[test]
    fn the_watcher_env_flag_accepts_exactly_four_spellings() {
        let _guard = test_lock();
        for on in ["1", "true", "TRUE", " yes ", "on", "On"] {
            assert!(watcher_env_disabled(Some(on)), "{on}");
        }
        for off in ["", "0", "false", "no", "off", "2", "enabled"] {
            assert!(!watcher_env_disabled(Some(off)), "{off}");
        }
        assert!(!watcher_env_disabled(None));
    }

    /// The lock probe: absent file, empty file, `<pid>\n<ts>`, PID-only, junk.
    #[test]
    fn the_lock_holder_is_the_first_line_or_nothing() {
        let _guard = test_lock();
        let dir = scratch_dir("lock");
        let path = dir.join("server.lock");
        let _ = std::fs::remove_file(&path);
        assert_eq!(read_lock_holder(&dir), None, "absent");

        std::fs::write(&path, "").expect("write");
        assert_eq!(read_lock_holder(&dir), None, "empty");

        std::fs::write(&path, "12345\n2026-01-01T00:00:00+00:00\n").expect("write");
        assert_eq!(read_lock_holder(&dir), Some(12_345));

        std::fs::write(&path, "999").expect("write");
        assert_eq!(
            read_lock_holder(&dir),
            Some(999),
            "PID-only, older versions"
        );

        std::fs::write(&path, "not-a-pid\nx").expect("write");
        assert_eq!(read_lock_holder(&dir), None);

        // The value reaches the payload as an integer, not a string.
        std::fs::write(&path, "4242\n").expect("write");
        let conn = Connection::open_in_memory().expect("store");
        let body = assemble_status(&conn, &dir, Some("1"), T0).expect("status");
        assert_eq!(body["watcher"]["lock_held_by"], Value::from(4242));
        assert_eq!(body["watcher"]["enabled"], Value::Bool(false));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn py_int_takes_the_forms_cpythons_int_takes() {
        let _guard = test_lock();
        assert_eq!(py_int("42"), Some(42));
        assert_eq!(py_int(" 42 "), Some(42));
        assert_eq!(py_int("-7"), Some(-7));
        assert_eq!(py_int("+7"), Some(7));
        assert_eq!(py_int("1_0"), Some(10));
        assert_eq!(py_int("1__0"), None);
        assert_eq!(py_int("_1"), None);
        assert_eq!(py_int("1_"), None);
        assert_eq!(py_int("4.2"), None);
        assert_eq!(py_int(""), None);
    }
}
