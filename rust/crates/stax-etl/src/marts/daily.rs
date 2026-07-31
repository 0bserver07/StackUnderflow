//! Port of `stackunderflow/etl/marts/daily.py` — the
//! `(day, project_id, provider, model, speed)` rollup.
//!
//! Additive: tokens, `message_count` and cost are summed into the existing row
//! by `ON CONFLICT DO UPDATE`, which is safe because the watermark guarantees no
//! `event_id` is processed twice.
//!
//! `session_count` is the exception, and the reason the second statement
//! exists. `COUNT(DISTINCT session_id)` is not additive across refresh windows:
//! a session producing events on day D in two windows would be counted twice
//! (1 + 1 = 2 instead of 1). Python follows option (a) from the spec — after the
//! additive upsert, recompute `session_count` from the *full* events table for
//! the keys this window touched. Both statements are ported verbatim, including
//! the row-value `IN` predicate in the UPDATE's WHERE.

use anyhow::Result;
use rusqlite::Connection;

use super::{MartBuilder, max_event_id};

/// Per-`(day, project, provider, model, speed)` cost + token rollup.
pub struct DailyMartBuilder;

impl MartBuilder for DailyMartBuilder {
    fn name(&self) -> &'static str {
        "daily"
    }

    fn refresh(&self, conn: &Connection, since_event_id: i64) -> Result<i64> {
        let max_id = max_event_id(conn)?;
        if max_id <= since_event_id {
            return Ok(since_event_id);
        }

        // ── additive upsert for SUM/COUNT(*) columns ──────────────────────
        conn.execute(
            r"
            INSERT INTO daily_mart (
                day, project_id, provider, model, speed,
                input_tokens, output_tokens, cache_read, cache_create,
                message_count, session_count, cost_usd
            )
            SELECT
                day, project_id, provider, model, speed,
                SUM(input_tokens),
                SUM(output_tokens),
                SUM(cache_read_tokens),
                SUM(cache_create_tokens),
                COUNT(*),
                COUNT(DISTINCT session_id),
                SUM(cost_usd)
            FROM usage_events
            WHERE id > ? AND id <= ?
            GROUP BY day, project_id, provider, model, speed
            ON CONFLICT (day, project_id, provider, model, speed) DO UPDATE SET
                input_tokens  = input_tokens  + excluded.input_tokens,
                output_tokens = output_tokens + excluded.output_tokens,
                cache_read    = cache_read    + excluded.cache_read,
                cache_create  = cache_create  + excluded.cache_create,
                message_count = message_count + excluded.message_count,
                session_count = session_count + excluded.session_count,
                cost_usd      = cost_usd      + excluded.cost_usd
            ",
            rusqlite::params![since_event_id, max_id],
        )?;

        // ── recompute session_count for affected keys ─────────────────────
        conn.execute(
            r"
            WITH affected AS (
                SELECT DISTINCT day, project_id, provider, model, speed
                FROM usage_events
                WHERE id > ? AND id <= ?
            ),
            recomputed AS (
                SELECT
                    e.day, e.project_id, e.provider, e.model, e.speed,
                    COUNT(DISTINCT e.session_id) AS sc
                FROM usage_events e
                JOIN affected a USING (day, project_id, provider, model, speed)
                GROUP BY e.day, e.project_id, e.provider, e.model, e.speed
            )
            UPDATE daily_mart
               SET session_count = (
                   SELECT sc FROM recomputed r
                   WHERE r.day = daily_mart.day
                     AND r.project_id = daily_mart.project_id
                     AND r.provider = daily_mart.provider
                     AND r.model = daily_mart.model
                     AND r.speed = daily_mart.speed
               )
             WHERE (day, project_id, provider, model, speed) IN (
                   SELECT day, project_id, provider, model, speed FROM affected
               )
            ",
            rusqlite::params![since_event_id, max_id],
        )?;

        Ok(max_id)
    }

    fn rebuild_from_scratch(&self, conn: &Connection) -> Result<()> {
        conn.execute("DELETE FROM daily_mart", [])?;
        self.refresh(conn, 0)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::testdb;
    use super::*;

    #[test]
    fn nothing_new_leaves_the_watermark_where_it_was() {
        let c = testdb::conn();
        assert_eq!(DailyMartBuilder.refresh(&c, 7).unwrap(), 7);
    }

    #[test]
    fn distinct_session_count_is_recomputed_not_summed_across_windows() {
        // The trap the second statement exists for: one session, two refresh
        // windows, same day. Naive addition would report 2.
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
            (1, 2, 0, 0),
            0.5,
        );
        assert_eq!(DailyMartBuilder.refresh(&c, 0).unwrap(), 1);

        testdb::event(
            &c,
            2,
            None,
            1,
            "s1",
            "claude",
            "m",
            "2026-01-01",
            (3, 4, 0, 0),
            0.25,
        );
        assert_eq!(DailyMartBuilder.refresh(&c, 1).unwrap(), 2);

        let (msgs, sessions, cost, tin): (i64, i64, f64, i64) = c
            .query_row(
                "SELECT message_count, session_count, cost_usd, input_tokens FROM daily_mart",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(msgs, 2);
        assert_eq!(sessions, 1, "COUNT(DISTINCT) must be recomputed, not added");
        assert!((cost - 0.75).abs() < 1e-12);
        assert_eq!(tin, 4);
    }

    #[test]
    fn a_rebuild_is_bit_reproducible_where_an_incremental_path_need_not_be() {
        // The wave-3 gate compares rebuild against rebuild, and this is why.
        // The additive path sums each window separately and then adds the
        // partial sums; a rebuild sums every row in one `SUM`. Float addition
        // is not associative, so the two can legitimately differ in the last
        // bits — but two rebuilds of the same events cannot.
        let c = testdb::conn();
        testdb::project(&c, 1, "p", "claude");
        let cost = |i: i64| 0.1 * f64::from(u32::try_from(i).unwrap());
        for i in 1..=3 {
            testdb::event(
                &c,
                i,
                None,
                1,
                "s1",
                "claude",
                "m",
                "2026-01-01",
                (i, i, 0, 0),
                cost(i),
            );
        }
        assert_eq!(DailyMartBuilder.refresh(&c, 0).unwrap(), 3);
        for i in 4..=6 {
            testdb::event(
                &c,
                i,
                None,
                1,
                "s2",
                "claude",
                "m",
                "2026-01-01",
                (i, i, 0, 0),
                cost(i),
            );
        }
        assert_eq!(DailyMartBuilder.refresh(&c, 3).unwrap(), 6);

        let (msgs, sessions): (i64, i64) = c
            .query_row(
                "SELECT message_count, session_count FROM daily_mart",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((msgs, sessions), (6, 2));

        DailyMartBuilder.rebuild_from_scratch(&c).unwrap();
        let first: f64 = c
            .query_row("SELECT cost_usd FROM daily_mart", [], |r| r.get(0))
            .unwrap();
        DailyMartBuilder.rebuild_from_scratch(&c).unwrap();
        let second: f64 = c
            .query_row("SELECT cost_usd FROM daily_mart", [], |r| r.get(0))
            .unwrap();
        assert_eq!(first.to_bits(), second.to_bits(), "{first} vs {second}");
    }
}
