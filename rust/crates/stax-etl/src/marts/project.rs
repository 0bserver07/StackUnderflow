//! Port of `stackunderflow/etl/marts/project.py` — one row per project.
//!
//! Three passes, in this order, and the order is the design:
//!
//! 1. **Events pass** — recompute the lifetime aggregate of every project with
//!    events in the window, `INSERT OR REPLACE` on the `project_id` primary key.
//!    The statement lists only the original 13 columns, so it *resets the dim
//!    columns to their DEFAULT* on every refresh.
//! 2. **Coverage seed** — a project with zero billable events can never win a
//!    row from a query driven `FROM usage_events`, so it stayed invisible to
//!    every mart-backed read path. Not a rare corner: a Claude `legacy-` history
//!    pseudo-session (user turns only, no token accounting), a provider whose
//!    adapter deliberately emits no usage events, and a normalizer that guards a
//!    row away all produce real projects with real messages and no events.
//! 3. **Dims pass** — [`super::dims::refresh_message_dims`] fills the twelve
//!    columns pass 1 just zeroed, for exactly the affected ids.
//!
//! # The seed's affected-guard is the load-bearing part
//!
//! [`seed_uncovered_projects`] returns **only** the ids it actually inserted,
//! computed by an anti-join *before* the INSERT. An already-covered project must
//! never re-enter `affected`, or every watcher cycle would re-run the full
//! `messages` scan of the dims pass for all 300-odd projects. On steady state
//! the seed contributes nothing and costs one indexed anti-join. The July
//! campaign built it this way deliberately; a port that recomputed `missing`
//! after the INSERT, or fed the INSERT's own row set into `affected`, would be
//! correct and unusably slow.

use anyhow::Result;
use rusqlite::Connection;

use super::{MartBuilder, max_event_id};

/// Per-project lifetime aggregates.
pub struct ProjectMartBuilder;

impl MartBuilder for ProjectMartBuilder {
    fn name(&self) -> &'static str {
        "project"
    }

    fn refresh(&self, conn: &Connection, since_event_id: i64) -> Result<i64> {
        let max_id = max_event_id(conn)?;
        // `affected` drives the second (message-dims) pass. Only ids whose mart
        // row this call actually (re)wrote may enter it.
        let mut affected: Vec<i64> = Vec::new();
        if max_id > since_event_id {
            affected = refresh_from_events(conn, since_event_id, max_id)?;
        }

        // Coverage seed — runs even when no new events arrived, so a project
        // that will never produce an event cannot stay invisible until some
        // unrelated event happens to land.
        affected.extend(seed_uncovered_projects(conn)?);

        super::dims::refresh_message_dims(conn, &affected)?;

        Ok(max_id.max(since_event_id))
    }

    fn rebuild_from_scratch(&self, conn: &Connection) -> Result<()> {
        // DELETE then a from-zero refresh, which runs the coverage seed too —
        // so a rebuild lands the same project set as the incremental path.
        conn.execute("DELETE FROM project_mart", [])?;
        self.refresh(conn, 0)?;
        Ok(())
    }
}

/// `project._refresh_from_events` — returns the affected project ids.
fn refresh_from_events(conn: &Connection, since_event_id: i64, max_id: i64) -> Result<Vec<i64>> {
    let affected: Vec<i64> = {
        let mut stmt =
            conn.prepare("SELECT DISTINCT project_id FROM usage_events WHERE id > ? AND id <= ?")?;
        stmt.query_map(rusqlite::params![since_event_id, max_id], |r| r.get(0))?
            .collect::<Result<Vec<i64>, _>>()?
    };

    conn.execute(
        r"
        INSERT OR REPLACE INTO project_mart (
            project_id, provider, slug, display_name,
            first_ts, last_ts,
            total_messages, total_sessions,
            total_input_tokens, total_output_tokens,
            total_cache_read, total_cache_create,
            total_cost_usd
        )
        SELECT
            e.project_id,
            p.provider,
            p.slug,
            p.display_name,
            MIN(e.ts),
            MAX(e.ts),
            COUNT(*),
            COUNT(DISTINCT e.session_id),
            SUM(e.input_tokens),
            SUM(e.output_tokens),
            SUM(e.cache_read_tokens),
            SUM(e.cache_create_tokens),
            SUM(e.cost_usd)
        FROM usage_events e
        JOIN projects p ON p.id = e.project_id
        WHERE e.project_id IN (
            SELECT DISTINCT project_id
            FROM usage_events
            WHERE id > ? AND id <= ?
        )
        GROUP BY e.project_id, p.provider, p.slug, p.display_name
        ",
        rusqlite::params![since_event_id, max_id],
    )?;
    Ok(affected)
}

/// `project._seed_uncovered_projects` — a zero-cost row for every uncovered
/// project, returning **only** the ids this call inserted.
///
/// The ids come from an anti-join computed *before* the INSERT, so a
/// steady-state call returns `[]` and never re-enters a project into the dims
/// pass. When the anti-join finds nothing the write is skipped entirely.
pub fn seed_uncovered_projects(conn: &Connection) -> Result<Vec<i64>> {
    let missing: Vec<i64> = {
        let mut stmt = conn.prepare(
            "SELECT p.id FROM projects p \
             LEFT JOIN project_mart m ON m.project_id = p.id \
             WHERE m.project_id IS NULL",
        )?;
        stmt.query_map([], |r| r.get(0))?
            .collect::<Result<Vec<i64>, _>>()?
    };
    if missing.is_empty() {
        return Ok(Vec::new());
    }
    // The INSERT deliberately offers every project and lets OR IGNORE drop the
    // covered ones — the anti-join above is what the caller learns from, not
    // this statement's row set.
    conn.execute(
        r"
        INSERT OR IGNORE INTO project_mart (
            project_id, provider, slug, display_name
        )
        SELECT p.id, p.provider, p.slug, p.display_name
        FROM projects p
        ",
        [],
    )?;
    Ok(missing)
}

#[cfg(test)]
mod tests {
    use super::super::testdb;
    use super::*;

    fn user_row(text: &str) -> String {
        format!(r#"{{"type":"human","message":{{"role":"user","content":"{text}"}}}}"#)
    }

    #[test]
    fn a_project_with_events_gets_totals_and_dims() {
        let c = testdb::conn();
        testdb::project(&c, 1, "alpha", "claude");
        testdb::session(&c, 1, 1, "s1");
        testdb::message(
            &c,
            1,
            1,
            0,
            "2026-01-01T00:00:00Z",
            "user",
            "go",
            "[]",
            &user_row("go"),
        );
        testdb::message(
            &c,
            2,
            1,
            1,
            "2026-01-01T00:01:00Z",
            "assistant",
            "ok",
            "[]",
            r#"{"type":"assistant","message":{"role":"assistant","content":[
                {"type":"tool_use","id":"a","name":"Read","input":{}}]}}"#,
        );
        testdb::event(
            &c,
            1,
            Some(2),
            1,
            "s1",
            "claude",
            "m",
            "2026-01-01",
            (10, 5, 2, 1),
            1.25,
        );

        ProjectMartBuilder.refresh(&c, 0).unwrap();

        let (msgs, sess, cost, u, a, tu, cmds, steps, tools): (
            i64,
            i64,
            f64,
            i64,
            i64,
            i64,
            i64,
            i64,
            i64,
        ) = c
            .query_row(
                "SELECT total_messages, total_sessions, total_cost_usd, total_user_messages, \
                 total_assistant_messages, total_tool_use_messages, total_commands, \
                 total_command_steps, total_command_tools FROM project_mart",
                [],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                        r.get(7)?,
                        r.get(8)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!((msgs, sess), (1, 1));
        assert!((cost - 1.25).abs() < 1e-12);
        assert_eq!((u, a, tu, cmds), (1, 1, 1, 1));
        assert_eq!((steps, tools), (1, 1));
    }

    #[test]
    fn an_event_less_project_is_seeded_with_zero_totals_and_real_dims() {
        // The coverage case: a history-only project. Totals are truthfully
        // zero; the dims still report its user-message and command counts.
        let c = testdb::conn();
        testdb::project(&c, 7, "history-only", "claude");
        testdb::session(&c, 1, 7, "legacy-s1");
        testdb::message(
            &c,
            1,
            1,
            0,
            "2026-01-01T00:00:00Z",
            "user",
            "a",
            "[]",
            &user_row("a"),
        );
        testdb::message(
            &c,
            2,
            1,
            1,
            "2026-01-01T00:01:00Z",
            "user",
            "b",
            "[]",
            &user_row("b"),
        );

        ProjectMartBuilder.refresh(&c, 0).unwrap();

        let (pid, slug, msgs, cost, u, cmds): (i64, String, i64, f64, i64, i64) = c
            .query_row(
                "SELECT project_id, slug, total_messages, total_cost_usd, \
                 total_user_messages, total_commands FROM project_mart",
                [],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!((pid, slug.as_str()), (7, "history-only"));
        assert_eq!(msgs, 0);
        assert!(cost.abs() < f64::EPSILON);
        assert_eq!((u, cmds), (2, 2));
    }

    #[test]
    fn the_seed_returns_only_ids_it_inserted() {
        // The anti-join guard. If the seed reported already-covered ids, every
        // steady-state refresh would re-scan every project's messages.
        let c = testdb::conn();
        testdb::project(&c, 1, "a", "claude");
        testdb::project(&c, 2, "b", "claude");
        let first = seed_uncovered_projects(&c).unwrap();
        assert_eq!(first, vec![1, 2]);
        let second = seed_uncovered_projects(&c).unwrap();
        assert!(second.is_empty(), "steady state must contribute nothing");

        // …and a newly appearing project is the only one reported.
        testdb::project(&c, 3, "c", "claude");
        assert_eq!(seed_uncovered_projects(&c).unwrap(), vec![3]);
    }

    #[test]
    fn the_events_pass_resets_the_dim_columns_and_the_second_pass_refills_them() {
        let c = testdb::conn();
        testdb::project(&c, 1, "a", "claude");
        testdb::session(&c, 1, 1, "s1");
        testdb::message(
            &c,
            1,
            1,
            0,
            "2026-01-01T00:00:00Z",
            "user",
            "go",
            "[]",
            &user_row("go"),
        );
        testdb::message(
            &c,
            2,
            1,
            1,
            "2026-01-01T00:01:00Z",
            "assistant",
            "ok",
            "[]",
            r#"{"type":"assistant","message":{"role":"assistant","content":"ok"}}"#,
        );
        testdb::event(
            &c,
            1,
            Some(2),
            1,
            "s1",
            "claude",
            "m",
            "2026-01-01",
            (1, 1, 0, 0),
            0.5,
        );
        ProjectMartBuilder.refresh(&c, 0).unwrap();

        // A second window over the same project re-writes the row (zeroing the
        // dims) and must refill them.
        testdb::event(
            &c,
            2,
            None,
            1,
            "s1",
            "claude",
            "m",
            "2026-01-02",
            (1, 1, 0, 0),
            0.5,
        );
        ProjectMartBuilder.refresh(&c, 1).unwrap();

        let (msgs, u, cmds): (i64, i64, i64) = c
            .query_row(
                "SELECT total_messages, total_user_messages, total_commands FROM project_mart",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(msgs, 2, "the totals are recomputed from ALL events");
        assert_eq!((u, cmds), (1, 1), "the dims must not be left at DEFAULT 0");
    }

    #[test]
    fn refresh_returns_the_high_water_mark_even_with_nothing_new() {
        let c = testdb::conn();
        testdb::project(&c, 1, "a", "claude");
        testdb::event(
            &c,
            5,
            None,
            1,
            "s1",
            "claude",
            "m",
            "2026-01-01",
            (1, 1, 0, 0),
            0.0,
        );
        assert_eq!(ProjectMartBuilder.refresh(&c, 0).unwrap(), 5);
        // `max(max_id, since_event_id)` — project is the one mart that does not
        // early-return, because the coverage seed must still run.
        assert_eq!(ProjectMartBuilder.refresh(&c, 9).unwrap(), 9);
    }

    #[test]
    fn a_rebuild_lands_the_same_project_set_as_the_incremental_path() {
        let c = testdb::conn();
        testdb::project(&c, 1, "with-events", "claude");
        testdb::project(&c, 2, "without", "claude");
        testdb::event(
            &c,
            1,
            None,
            1,
            "s1",
            "claude",
            "m",
            "2026-01-01",
            (1, 1, 0, 0),
            1.0,
        );
        ProjectMartBuilder.refresh(&c, 0).unwrap();
        let before: i64 = c
            .query_row("SELECT COUNT(*) FROM project_mart", [], |r| r.get(0))
            .unwrap();
        ProjectMartBuilder.rebuild_from_scratch(&c).unwrap();
        let after: i64 = c
            .query_row("SELECT COUNT(*) FROM project_mart", [], |r| r.get(0))
            .unwrap();
        assert_eq!((before, after), (2, 2));
    }
}
