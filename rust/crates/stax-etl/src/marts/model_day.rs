//! Port of `python-legacy: etl/marts/model_day.py` — the `(day, model, speed)`
//! rollup for the compare-across-agents view.
//!
//! Additive over the SUM/COUNT(*) columns; `session_count` is recomputed for
//! affected keys after the upsert, for the reason `daily` documents at length.

use anyhow::Result;
use rusqlite::Connection;

use super::{MartBuilder, max_event_id};

/// Per-`(day, model, speed)` rollup across all providers + projects.
pub struct ModelDayMartBuilder;

impl MartBuilder for ModelDayMartBuilder {
    fn name(&self) -> &'static str {
        "model_day"
    }

    fn refresh(&self, conn: &Connection, since_event_id: i64) -> Result<i64> {
        let max_id = max_event_id(conn)?;
        if max_id <= since_event_id {
            return Ok(since_event_id);
        }

        conn.execute(
            r"
            INSERT INTO model_day_mart (
                day, model, speed,
                cost_usd, input_tokens, output_tokens,
                cache_read, cache_create,
                message_count, session_count
            )
            SELECT
                day, model, speed,
                SUM(cost_usd),
                SUM(input_tokens),
                SUM(output_tokens),
                SUM(cache_read_tokens),
                SUM(cache_create_tokens),
                COUNT(*),
                COUNT(DISTINCT session_id)
            FROM usage_events
            WHERE id > ? AND id <= ?
            GROUP BY day, model, speed
            ON CONFLICT (day, model, speed) DO UPDATE SET
                cost_usd      = cost_usd      + excluded.cost_usd,
                input_tokens  = input_tokens  + excluded.input_tokens,
                output_tokens = output_tokens + excluded.output_tokens,
                cache_read    = cache_read    + excluded.cache_read,
                cache_create  = cache_create  + excluded.cache_create,
                message_count = message_count + excluded.message_count,
                session_count = session_count + excluded.session_count
            ",
            rusqlite::params![since_event_id, max_id],
        )?;

        conn.execute(
            r"
            WITH affected AS (
                SELECT DISTINCT day, model, speed
                FROM usage_events
                WHERE id > ? AND id <= ?
            ),
            recomputed AS (
                SELECT
                    e.day, e.model, e.speed,
                    COUNT(DISTINCT e.session_id) AS sc
                FROM usage_events e
                JOIN affected a USING (day, model, speed)
                GROUP BY e.day, e.model, e.speed
            )
            UPDATE model_day_mart
               SET session_count = (
                   SELECT sc FROM recomputed r
                   WHERE r.day = model_day_mart.day
                     AND r.model = model_day_mart.model
                     AND r.speed = model_day_mart.speed
               )
             WHERE (day, model, speed) IN (
                   SELECT day, model, speed FROM affected
               )
            ",
            rusqlite::params![since_event_id, max_id],
        )?;

        Ok(max_id)
    }

    fn rebuild_from_scratch(&self, conn: &Connection) -> Result<()> {
        conn.execute("DELETE FROM model_day_mart", [])?;
        self.refresh(conn, 0)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::testdb;
    use super::*;

    #[test]
    fn the_key_spans_projects_and_providers() {
        let c = testdb::conn();
        testdb::project(&c, 1, "a", "claude");
        testdb::project(&c, 2, "b", "codex");
        testdb::event(
            &c,
            1,
            None,
            1,
            "s1",
            "claude",
            "opus",
            "2026-01-01",
            (1, 1, 0, 0),
            1.0,
        );
        testdb::event(
            &c,
            2,
            None,
            2,
            "s2",
            "codex",
            "opus",
            "2026-01-01",
            (2, 2, 0, 0),
            2.0,
        );
        ModelDayMartBuilder.refresh(&c, 0).unwrap();

        let (rows, msgs, sc, cost): (i64, i64, i64, f64) = c
            .query_row(
                "SELECT COUNT(*), SUM(message_count), SUM(session_count), SUM(cost_usd) \
                 FROM model_day_mart",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!((rows, msgs, sc), (1, 2, 2));
        assert!((cost - 3.0).abs() < 1e-12);
    }

    #[test]
    fn session_count_is_recomputed_across_windows() {
        let c = testdb::conn();
        testdb::project(&c, 1, "a", "claude");
        testdb::event(
            &c,
            1,
            None,
            1,
            "s1",
            "claude",
            "opus",
            "2026-01-01",
            (1, 1, 0, 0),
            0.0,
        );
        ModelDayMartBuilder.refresh(&c, 0).unwrap();
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
        ModelDayMartBuilder.refresh(&c, 1).unwrap();
        let sc: i64 = c
            .query_row("SELECT session_count FROM model_day_mart", [], |r| r.get(0))
            .unwrap();
        assert_eq!(sc, 1);
    }
}
