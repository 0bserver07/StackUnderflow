//! Port of `python-legacy: etl/marts/session.py` — one row per session.
//!
//! Replace-from-scratch-for-affected-keys: new events for an existing session
//! invalidate the prior aggregate, so the row is recomputed from *all* of that
//! session's events and swapped atomically by `INSERT OR REPLACE` on the
//! `session_id` primary key.
//!
//! `primary_model` is a correlated subquery — the assistant model with the most
//! messages in the session — so a session that switches models mid-conversation
//! still gets a stable label. Ties resolve by SQL's natural row order:
//! deterministic for a given plan, unspecified across them. That is Python's
//! recorded position on it ("fine for a tie-breaker on a 'primarily X' label"),
//! and it is also a thing the wave-3 gate can catch, because the two sides run
//! different SQLite builds.
//!
//! The `WHERE e.session_id IN (SELECT DISTINCT session_id …)` shape is the §6b
//! list-subquery idiom. It looks like it wants to be a JOIN; the measured
//! difference on this store is 9ms against 912ms.

use anyhow::Result;
use rusqlite::Connection;

use super::{MartBuilder, max_event_id};

/// Per-session lifetime aggregates.
pub struct SessionMartBuilder;

impl MartBuilder for SessionMartBuilder {
    fn name(&self) -> &'static str {
        "session"
    }

    fn refresh(&self, conn: &Connection, since_event_id: i64) -> Result<i64> {
        let max_id = max_event_id(conn)?;
        if max_id <= since_event_id {
            return Ok(since_event_id);
        }

        conn.execute(
            r"
            INSERT OR REPLACE INTO session_mart (
                session_id, project_id, provider, primary_model,
                first_ts, last_ts,
                message_count, user_message_count, assistant_message_count,
                input_tokens, output_tokens, cache_read, cache_create,
                cost_usd, is_one_shot, cwd
            )
            SELECT
                e.session_id,
                MIN(e.project_id),
                MIN(e.provider),
                (
                    SELECT e2.model
                    FROM usage_events e2
                    WHERE e2.session_id = e.session_id
                      AND e2.role = 'assistant'
                      AND e2.model <> ''
                    GROUP BY e2.model
                    ORDER BY COUNT(*) DESC
                    LIMIT 1
                ) AS primary_model,
                MIN(e.ts),
                MAX(e.ts),
                COUNT(*),
                SUM(CASE WHEN e.role = 'user' THEN 1 ELSE 0 END),
                SUM(CASE WHEN e.role = 'assistant' THEN 1 ELSE 0 END),
                SUM(e.input_tokens),
                SUM(e.output_tokens),
                SUM(e.cache_read_tokens),
                SUM(e.cache_create_tokens),
                SUM(e.cost_usd),
                CASE
                    WHEN SUM(CASE WHEN e.role = 'user' THEN 1 ELSE 0 END) = 1
                     AND SUM(CASE WHEN e.role = 'assistant' THEN 1 ELSE 0 END) = 1
                    THEN 1
                    ELSE 0
                END,
                NULL  -- cwd: deferred to a future wave
            FROM usage_events e
            WHERE e.session_id IN (
                SELECT DISTINCT session_id
                FROM usage_events
                WHERE id > ? AND id <= ?
            )
            GROUP BY e.session_id
            ",
            rusqlite::params![since_event_id, max_id],
        )?;

        Ok(max_id)
    }

    fn rebuild_from_scratch(&self, conn: &Connection) -> Result<()> {
        conn.execute("DELETE FROM session_mart", [])?;
        self.refresh(conn, 0)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::testdb;
    use super::*;

    #[test]
    fn a_session_row_is_recomputed_whole_not_added_to() {
        let c = testdb::conn();
        testdb::project(&c, 1, "p", "claude");
        testdb::event(
            &c,
            1,
            None,
            1,
            "s1",
            "claude",
            "opus",
            "2026-01-01",
            (10, 1, 0, 0),
            1.0,
        );
        SessionMartBuilder.refresh(&c, 0).unwrap();
        testdb::event(
            &c,
            2,
            None,
            1,
            "s1",
            "claude",
            "opus",
            "2026-01-02",
            (20, 2, 0, 0),
            2.0,
        );
        SessionMartBuilder.refresh(&c, 1).unwrap();

        let (n, tin, cost, model, first, last): (i64, i64, f64, String, String, String) = c
            .query_row(
                "SELECT message_count, input_tokens, cost_usd, primary_model, first_ts, last_ts \
                 FROM session_mart",
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
        assert_eq!((n, tin), (2, 30));
        assert!((cost - 3.0).abs() < 1e-12);
        assert_eq!(model, "opus");
        assert_eq!(first, "2026-01-01T00:00:00Z");
        assert_eq!(last, "2026-01-02T00:00:00Z");
    }

    #[test]
    fn one_shot_is_exactly_one_user_and_one_assistant_event() {
        let c = testdb::conn();
        testdb::project(&c, 1, "p", "claude");
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
            0.0,
        );
        c.execute("UPDATE usage_events SET role='user' WHERE id=1", [])
            .unwrap();
        testdb::event(
            &c,
            2,
            None,
            1,
            "s1",
            "claude",
            "m",
            "2026-01-01",
            (1, 1, 0, 0),
            0.0,
        );
        SessionMartBuilder.refresh(&c, 0).unwrap();
        let (one_shot, u, a): (i64, i64, i64) = c
            .query_row(
                "SELECT is_one_shot, user_message_count, assistant_message_count FROM session_mart",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!((one_shot, u, a), (1, 1, 1));
    }

    #[test]
    fn primary_model_is_the_most_used_assistant_model() {
        let c = testdb::conn();
        testdb::project(&c, 1, "p", "claude");
        testdb::event(
            &c,
            1,
            None,
            1,
            "s1",
            "claude",
            "haiku",
            "2026-01-01",
            (1, 1, 0, 0),
            0.0,
        );
        testdb::event(
            &c,
            2,
            None,
            1,
            "s1",
            "claude",
            "opus",
            "2026-01-01",
            (1, 1, 0, 0),
            0.0,
        );
        testdb::event(
            &c,
            3,
            None,
            1,
            "s1",
            "claude",
            "opus",
            "2026-01-01",
            (1, 1, 0, 0),
            0.0,
        );
        SessionMartBuilder.refresh(&c, 0).unwrap();
        let model: String = c
            .query_row("SELECT primary_model FROM session_mart", [], |r| r.get(0))
            .unwrap();
        assert_eq!(model, "opus");
    }

    #[test]
    fn cwd_is_still_null_in_v1() {
        let c = testdb::conn();
        testdb::project(&c, 1, "p", "claude");
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
            0.0,
        );
        SessionMartBuilder.refresh(&c, 0).unwrap();
        let cwd: Option<String> = c
            .query_row("SELECT cwd FROM session_mart", [], |r| r.get(0))
            .unwrap();
        assert!(cwd.is_none());
    }
}
