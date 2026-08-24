//! Port of `python-legacy: etl/marts/provider_day.py` — the `(day, provider)`
//! rollup behind the by-provider chart.
//!
//! Additive over cost + `message_count`, with the same non-additive-DISTINCT
//! caveat as `daily`: both `session_count` and `project_count` are recomputed
//! for the keys this window touched.

use anyhow::Result;
use rusqlite::Connection;

use super::{MartBuilder, max_event_id};

/// Per-`(day, provider)` cost + count rollup.
pub struct ProviderDayMartBuilder;

impl MartBuilder for ProviderDayMartBuilder {
    fn name(&self) -> &'static str {
        "provider_day"
    }

    fn refresh(&self, conn: &Connection, since_event_id: i64) -> Result<i64> {
        let max_id = max_event_id(conn)?;
        if max_id <= since_event_id {
            return Ok(since_event_id);
        }

        conn.execute(
            r"
            INSERT INTO provider_day_mart (
                day, provider, cost_usd, message_count,
                session_count, project_count
            )
            SELECT
                day, provider,
                SUM(cost_usd),
                COUNT(*),
                COUNT(DISTINCT session_id),
                COUNT(DISTINCT project_id)
            FROM usage_events
            WHERE id > ? AND id <= ?
            GROUP BY day, provider
            ON CONFLICT (day, provider) DO UPDATE SET
                cost_usd      = cost_usd      + excluded.cost_usd,
                message_count = message_count + excluded.message_count,
                session_count = session_count + excluded.session_count,
                project_count = project_count + excluded.project_count
            ",
            rusqlite::params![since_event_id, max_id],
        )?;

        // Recompute the two DISTINCT-count columns for affected keys.
        conn.execute(
            r"
            WITH affected AS (
                SELECT DISTINCT day, provider
                FROM usage_events
                WHERE id > ? AND id <= ?
            ),
            recomputed AS (
                SELECT
                    e.day, e.provider,
                    COUNT(DISTINCT e.session_id) AS sc,
                    COUNT(DISTINCT e.project_id) AS pc
                FROM usage_events e
                JOIN affected a USING (day, provider)
                GROUP BY e.day, e.provider
            )
            UPDATE provider_day_mart
               SET session_count = (
                       SELECT sc FROM recomputed r
                       WHERE r.day = provider_day_mart.day
                         AND r.provider = provider_day_mart.provider
                   ),
                   project_count = (
                       SELECT pc FROM recomputed r
                       WHERE r.day = provider_day_mart.day
                         AND r.provider = provider_day_mart.provider
                   )
             WHERE (day, provider) IN (
                   SELECT day, provider FROM affected
               )
            ",
            rusqlite::params![since_event_id, max_id],
        )?;

        Ok(max_id)
    }

    fn rebuild_from_scratch(&self, conn: &Connection) -> Result<()> {
        conn.execute("DELETE FROM provider_day_mart", [])?;
        self.refresh(conn, 0)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::testdb;
    use super::*;

    #[test]
    fn both_distinct_columns_are_recomputed_across_windows() {
        let c = testdb::conn();
        testdb::project(&c, 1, "a", "claude");
        testdb::project(&c, 2, "b", "claude");
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
        ProviderDayMartBuilder.refresh(&c, 0).unwrap();
        // Same session AND same project again — neither DISTINCT count moves.
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
            1.0,
        );
        // …plus a genuinely new project.
        testdb::event(
            &c,
            3,
            None,
            2,
            "s2",
            "claude",
            "m",
            "2026-01-01",
            (1, 1, 0, 0),
            1.0,
        );
        ProviderDayMartBuilder.refresh(&c, 1).unwrap();

        let (msgs, sc, pc, cost): (i64, i64, i64, f64) = c
            .query_row(
                "SELECT message_count, session_count, project_count, cost_usd \
                 FROM provider_day_mart",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!((msgs, sc, pc), (3, 2, 2));
        assert!((cost - 3.0).abs() < 1e-12);
    }
}
