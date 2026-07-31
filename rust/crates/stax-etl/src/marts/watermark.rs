//! Port of `stackunderflow/etl/watermark.py`.
//!
//! Each mart keeps an independent `last_event_id` so incremental refresh
//! resumes where the last run stopped, and a partial failure of one mart cannot
//! strand another.

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension};

use super::all;

/// `watermark.get_watermark` — the current `last_event_id`, `0` when unset.
///
/// `optional()` rather than `.ok()`: "no such mart" is `0`, but a genuine SQL
/// error must not be laundered into one.
pub fn get_watermark(conn: &Connection, mart_name: &str) -> Result<i64> {
    let v: Option<i64> = conn
        .query_row(
            "SELECT last_event_id FROM mart_watermark WHERE mart_name = ?",
            [mart_name],
            |r| r.get(0),
        )
        .optional()?;
    Ok(v.unwrap_or(0))
}

/// `watermark.set_watermark` — upsert, stamping `last_refresh_ts` with now.
///
/// The update is **monotonic**: `MAX(mart_watermark.last_event_id,
/// excluded.last_event_id)`. That is not decoration — Python's docstring
/// records the production failure it fixes, where a server-startup refresh
/// working from an older event snapshot committed a stale value over a faster
/// concurrent writer's and silently forced a full re-scan. Deliberate resets
/// (`etl backfill --force`) DELETE the row first, so the INSERT path is
/// unaffected by the guard.
///
/// `now` is injected rather than read from the clock: `set_var` is unsafe under
/// Rust 2024 and this workspace forbids `unsafe`, so the campaign's settings
/// pattern is pure-function-plus-injection throughout (ARCHITECT-STATE finding
/// 5). It is also the only way a watermark test can assert on the stamp.
pub fn set_watermark(
    conn: &Connection,
    mart_name: &str,
    last_event_id: i64,
    now: &str,
) -> Result<()> {
    conn.execute(
        "INSERT INTO mart_watermark (mart_name, last_event_id, last_refresh_ts) \
         VALUES (?, ?, ?) \
         ON CONFLICT(mart_name) DO UPDATE SET \
             last_event_id = MAX(mart_watermark.last_event_id, excluded.last_event_id), \
             last_refresh_ts = excluded.last_refresh_ts",
        rusqlite::params![mart_name, last_event_id, now],
    )?;
    Ok(())
}

/// `watermark.refresh_all_marts` — refresh every registered mart from its
/// watermark and persist the result.
///
/// Returns `(mart_name, events_processed)` in registry order, where
/// `events_processed = max(0, new - old)`. Python returns a `dict`, which is
/// insertion-ordered over the same registry, so the sequence is the same.
pub fn refresh_all_marts(conn: &Connection, now: &str) -> Result<Vec<(String, i64)>> {
    let mut out = Vec::new();
    for mart in all() {
        let name = mart.name();
        let old = get_watermark(conn, name)?;
        let new = mart.refresh(conn, old)?;
        set_watermark(conn, name, new, now)?;
        out.push((name.to_string(), (new - old).max(0)));
    }
    Ok(out)
}

/// The wave-3 gate's rebuild path: wipe every mart and rebuild it from event 0.
///
/// This is `etl/backfill.py::_drop_events_and_marts` **minus the two event-side
/// DELETEs** — the marts are rebuilt from the `usage_events` already in the
/// store rather than from a re-normalised event stream. That separation is what
/// makes the gate a *mart* gate: `usage_events.cost_usd` is frozen at
/// normalisation time (DIV-001), so leaving the events untouched holds the rate
/// card fixed by construction and no price-book state — primed or unprimed
/// (DIV-016) — can reach a mart column.
///
/// The watermark is stamped afterwards with the value `refresh` returned, which
/// is what `refresh_all_marts` would have persisted; `rebuild_from_scratch`
/// itself does not touch `mart_watermark` in Python either.
pub fn rebuild_all_marts(conn: &Connection, now: &str) -> Result<Vec<(String, i64)>> {
    rebuild_all_marts_with(conn, now, |_, _, _| {})
}

/// [`rebuild_all_marts`] with a per-mart progress callback.
///
/// A full rebuild of the maintainer's store is a ~40-minute single-threaded
/// grind — `tool_mart`'s session-count recompute and `command_mart`'s per-event
/// prompt walk are both documented as unbounded by the watermark window — and a
/// run with no output is indistinguishable from a hang. The callback receives
/// `(mart_name, high_water_mark, seconds)` as each mart lands.
pub fn rebuild_all_marts_with(
    conn: &Connection,
    now: &str,
    mut progress: impl FnMut(&str, i64, f64),
) -> Result<Vec<(String, i64)>> {
    conn.execute("DELETE FROM mart_watermark", [])?;
    let mut out = Vec::new();
    for mart in all() {
        let name = mart.name();
        let started = std::time::Instant::now();
        mart.rebuild_from_scratch(conn)?;
        let high = super::max_event_id(conn)?;
        set_watermark(conn, name, high, now)?;
        progress(name, high, started.elapsed().as_secs_f64());
        out.push((name.to_string(), high));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::super::testdb;
    use super::*;

    #[test]
    fn a_missing_watermark_reads_zero() {
        let c = testdb::conn();
        assert_eq!(get_watermark(&c, "daily").unwrap(), 0);
    }

    #[test]
    fn the_watermark_update_is_monotonic() {
        let c = testdb::conn();
        set_watermark(&c, "daily", 100, "t1").unwrap();
        set_watermark(&c, "daily", 40, "t2").unwrap();
        assert_eq!(get_watermark(&c, "daily").unwrap(), 100);
        // …but the timestamp still moves, exactly as Python's does.
        let ts: String = c
            .query_row(
                "SELECT last_refresh_ts FROM mart_watermark WHERE mart_name='daily'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(ts, "t2");
        // A deliberate reset DELETEs first, so the guard cannot pin it high.
        c.execute("DELETE FROM mart_watermark", []).unwrap();
        set_watermark(&c, "daily", 5, "t3").unwrap();
        assert_eq!(get_watermark(&c, "daily").unwrap(), 5);
    }

    #[test]
    fn refresh_all_reports_zero_for_every_mart_on_an_empty_store() {
        let c = testdb::conn();
        let out = refresh_all_marts(&c, "t").unwrap();
        assert_eq!(out.len(), 8);
        assert!(out.iter().all(|(_, n)| *n == 0));
    }

    #[test]
    fn refresh_all_is_idempotent() {
        let c = testdb::conn();
        testdb::project(&c, 1, "p", "claude");
        testdb::session(&c, 1, 1, "s1");
        testdb::message(
            &c,
            1,
            1,
            0,
            "2026-01-01T00:00:00Z",
            "assistant",
            "",
            "[]",
            "{}",
        );
        testdb::event(
            &c,
            1,
            Some(1),
            1,
            "s1",
            "claude",
            "m",
            "2026-01-01",
            (5, 7, 0, 0),
            1.5,
        );

        let first = refresh_all_marts(&c, "t").unwrap();
        assert_eq!(first.iter().find(|(n, _)| n == "daily").unwrap().1, 1);
        let cost_after_one: f64 = c
            .query_row("SELECT SUM(cost_usd) FROM daily_mart", [], |r| r.get(0))
            .unwrap();

        let second = refresh_all_marts(&c, "t").unwrap();
        assert!(second.iter().all(|(_, n)| *n == 0));
        let cost_after_two: f64 = c
            .query_row("SELECT SUM(cost_usd) FROM daily_mart", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            cost_after_one, cost_after_two,
            "a second refresh must not re-add the same window"
        );
    }
}
