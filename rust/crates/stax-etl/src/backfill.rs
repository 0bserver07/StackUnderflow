//! `etl/backfill.py` (365 ln) — the backfill orchestrator.
//!
//! | Item | Python | Consumed by |
//! |---|---|---|
//! | [`backfill`] | `backfill.backfill` | `routes/etl.rs` (the background task), `stax-cli`'s `etl backfill` |
//! | [`drop_events_and_marts`] | `backfill._drop_events_and_marts` | [`backfill`] |
//!
//! # Why this file is here and not in `stax-server`
//!
//! Batch E ported `etl/backfill.py` and `etl/backfill_jobs.py` into ONE module
//! inside `stax-server`, which was right while the only caller was
//! `POST /api/etl/backfill`. It is not right now: `cli.py`'s `etl backfill`
//! verb calls `stackunderflow.etl.backfill` directly and never touches the job
//! slot, and `stax-cli` may not link `stax-server` (DIV-279). So the two Python
//! files are two Rust modules again — the orchestrator here, in the crate whose
//! charter is `etl/`, and the process-local slot (`backfill_jobs.py`, which
//! genuinely is server state) still in `stax_server::services::etl_backfill`,
//! which re-exports these three names so no route path changed.
//!
//! # The events half is already ported; this is the orchestrator
//!
//! `backfill._run_normalizers` — the streaming keyset walk, the per-chunk
//! transaction, `INSERT OR IGNORE` against `uniq_events_msg`, the WAL checkpoint
//! and the poison-row swallow — is [`crate::normalize::pass::run`], written for
//! RS-3 and pinned by its own tests. Re-transliterating it here would fork it.
//! What that file explicitly did *not* take, by the design note at its top, is
//! exactly what lives here: the `force` wipe, the empty-registry short circuit,
//! and the `refresh_all_marts` call that follows the pass.
//!
//! # `_drop_events_and_marts` is NOT `watermark::rebuild_all_marts`
//!
//! They look interchangeable and are not. `rebuild_all_marts` **stamps a
//! watermark per mart** (`set_watermark(conn, name, max_event_id, now)`);
//! Python's `_drop_events_and_marts` stamps nothing — it `DELETE`s
//! `mart_watermark` wholesale and then calls each builder's
//! `rebuild_from_scratch`, which is `DELETE FROM <name>_mart` + `refresh(conn,
//! 0)` and never touches the watermark table. Reusing `rebuild_all_marts` would
//! leave eight `mart_watermark` rows stamped at the pre-wipe high-water mark
//! *before* the normalize pass re-created the events, so the `refresh_all_marts`
//! that follows would skip every event it had just written and every mart would
//! come back empty. That is the DIV-148-shaped trap in this file: two
//! nearly-identical helpers, one correct.
//! [`tests::the_force_wipe_leaves_no_watermark_behind`] pins it.
//!
//! # The price book seam runs through the CALLER's `NormalizeContext`
//!
//! `stackunderflow etl backfill` (the CLI) runs unprimed — `use_price_book_store`
//! is only ever called by `server.py`'s lifespan — so the CLI verb builds its
//! context from the manifest and the route builds one from `crate::pricing::
//! engine`. Both are correct and they differ; that is DIV-016 / RS-3-082, and
//! it is the reason the context is a parameter here rather than a constant.

use anyhow::Result;
use rusqlite::Connection;

use crate::marts;
use crate::marts::watermark::refresh_all_marts;
use crate::normalize::NormalizeContext;
use crate::normalize::pass;

/// `BackfillReport` — what one [`backfill`] call did.
///
/// Nothing on the wire reads this: `POST /api/etl/backfill` answers `202` with
/// the job id long before the run finishes, and the background worker discards
/// the report exactly as Python's `_run_backfill_in_background` does. It is
/// returned anyway because the CLI verb (wave 8) is the other caller of this
/// function and does render it.
#[derive(Debug, Clone, Default)]
pub struct BackfillReport {
    /// Rows written to `usage_events`.
    pub events_inserted: u64,
    /// Rows `uniq_events_msg` rejected as already-converted.
    pub events_skipped_duplicate: u64,
    /// `messages` rows streamed.
    pub messages_seen: u64,
    /// `{mart_name: events_processed}` in registry order.
    pub marts_refreshed: Vec<(String, i64)>,
    /// `time.perf_counter()` delta across the whole call.
    pub duration_seconds: f64,
}

/// `_drop_events_and_marts` — the `force=True` wipe.
///
/// Order is load-bearing and documented as such in Python: events first, so a
/// mart's `rebuild_from_scratch` cannot repopulate against rows that are about
/// to be deleted; then `mart_watermark`; then every **registered** mart — all
/// eight, not the five `KNOWN_MART_NAMES` the status surface renders.
///
/// # Errors
/// Any SQLite error.
pub fn drop_events_and_marts(conn: &Connection) -> Result<()> {
    conn.execute("DELETE FROM usage_events", [])?;
    conn.execute("DELETE FROM mart_watermark", [])?;
    for mart in marts::all() {
        mart.rebuild_from_scratch(conn)?;
    }
    Ok(())
}

/// `backfill(conn, force=…)` — convert every `messages` row into
/// `usage_events`, then refresh every mart.
///
/// `now` is the `last_refresh_ts` stamp `refresh_all_marts` writes. Python
/// re-reads the clock once **per mart** (`set_watermark` calls
/// `datetime.now(UTC)` itself) where this passes one value for all eight; see
/// the numbered finding in `parity/DIV-e-etl.md`.
///
/// # Errors
/// Any SQLite error from the wipe, the pass, or a mart refresh. Python lets the
/// same errors propagate out of `backfill` into its caller's `except`.
pub fn backfill(
    conn: &Connection,
    ctx: &NormalizeContext,
    force: bool,
    now: &str,
) -> Result<BackfillReport> {
    let started = std::time::Instant::now();
    let mut report = BackfillReport::default();

    if force {
        drop_events_and_marts(conn)?;
    }

    // `if not normalizers:` — the registry is a twenty-entry `const` array here,
    // so this branch cannot be reached. Kept because reaching it in Python skips
    // the pass and *still* calls `refresh_all_marts` ("so empty marts can
    // finalize their watermarks"), and a port that silently dropped the
    // distinction would be one refactor away from dropping the call too.
    if crate::normalize::all().is_empty() {
        report.marts_refreshed = refresh_all_marts(conn, now)?;
        report.duration_seconds = started.elapsed().as_secs_f64();
        return Ok(report);
    }

    let pass_report = pass::run(conn, ctx)?;
    report.events_inserted = pass_report.events_inserted;
    report.events_skipped_duplicate = pass_report.events_skipped_duplicate;
    report.messages_seen = pass_report.messages_seen;

    report.marts_refreshed = refresh_all_marts(conn, now)?;
    report.duration_seconds = started.elapsed().as_secs_f64();
    Ok(report)
}

#[doc(hidden)]
pub mod testdb {
    //! A v030-shaped store for the ETL modules that read `usage_events`.
    //!
    //! `#[doc(hidden)] pub` rather than `#[cfg(test)] pub(crate)`: the status
    //! assembler moved to `stax-reports` and its tests seed the same store, and
    //! a `#[cfg(test)]` module is invisible across a crate boundary. The
    //! alternative was a second copy of this DDL, which is a forked fixture —
    //! the thing `assemble_worktrees_payload` cost the campaign once already.
    //!
    //! The DDL is `stax_etl::marts::testdb::SCHEMA`, which is itself copied from
    //! `stackunderflow/store/migrations/` (v006, v007, v011, v012, v022, v023,
    //! v025, v030) — but that module is `#[cfg(test)] pub(crate)` to *its* crate
    //! and cannot be reached from here. Four columns the mart tests do not need
    //! are added back because the status assembler reads them:
    //! `usage_events.cost_source` is half of its `GROUP BY`, and `account` /
    //! `reasoning_tokens` / `raw_extras` are bound by `insert_event`.

    use rusqlite::Connection;

    pub const SCHEMA: &str = r"
        CREATE TABLE projects (
            id INTEGER PRIMARY KEY, provider TEXT NOT NULL, slug TEXT NOT NULL,
            display_name TEXT NOT NULL, UNIQUE (provider, slug));
        CREATE TABLE sessions (
            id INTEGER PRIMARY KEY, project_id INTEGER NOT NULL, session_id TEXT NOT NULL);
        CREATE TABLE messages (
            id INTEGER PRIMARY KEY, session_fk INTEGER NOT NULL, seq INTEGER NOT NULL,
            timestamp TEXT, role TEXT, model TEXT,
            input_tokens INTEGER DEFAULT 0, output_tokens INTEGER DEFAULT 0,
            cache_read_tokens INTEGER DEFAULT 0, cache_create_tokens INTEGER DEFAULT 0,
            content_text TEXT, tools_json TEXT, raw_json TEXT,
            is_sidechain INTEGER DEFAULT 0, uuid TEXT, parent_uuid TEXT,
            speed TEXT DEFAULT 'standard');
        CREATE TABLE usage_events (
            id INTEGER PRIMARY KEY, source_message_fk INTEGER, project_id INTEGER NOT NULL,
            session_id TEXT NOT NULL, provider TEXT NOT NULL, model TEXT NOT NULL DEFAULT '',
            speed TEXT NOT NULL DEFAULT 'standard', role TEXT NOT NULL DEFAULT 'assistant',
            ts TEXT NOT NULL, day TEXT NOT NULL,
            account TEXT NOT NULL DEFAULT 'default',
            input_tokens INTEGER NOT NULL DEFAULT 0, output_tokens INTEGER NOT NULL DEFAULT 0,
            cache_read_tokens INTEGER NOT NULL DEFAULT 0,
            cache_create_tokens INTEGER NOT NULL DEFAULT 0,
            reasoning_tokens INTEGER NOT NULL DEFAULT 0,
            cost_usd REAL NOT NULL DEFAULT 0.0,
            cost_source TEXT NOT NULL DEFAULT 'rate_card',
            raw_extras TEXT);
        CREATE UNIQUE INDEX uniq_events_msg ON usage_events(source_message_fk);
        CREATE TABLE daily_mart (
            day TEXT NOT NULL, project_id INTEGER NOT NULL, provider TEXT NOT NULL,
            model TEXT NOT NULL DEFAULT '', speed TEXT NOT NULL DEFAULT 'standard',
            input_tokens INTEGER NOT NULL DEFAULT 0, output_tokens INTEGER NOT NULL DEFAULT 0,
            cache_read INTEGER NOT NULL DEFAULT 0, cache_create INTEGER NOT NULL DEFAULT 0,
            message_count INTEGER NOT NULL DEFAULT 0, session_count INTEGER NOT NULL DEFAULT 0,
            cost_usd REAL NOT NULL DEFAULT 0.0,
            PRIMARY KEY (day, project_id, provider, model, speed));
        CREATE TABLE session_mart (
            session_id TEXT PRIMARY KEY, project_id INTEGER NOT NULL, provider TEXT NOT NULL,
            primary_model TEXT, first_ts TEXT NOT NULL, last_ts TEXT NOT NULL,
            message_count INTEGER NOT NULL DEFAULT 0,
            user_message_count INTEGER NOT NULL DEFAULT 0,
            assistant_message_count INTEGER NOT NULL DEFAULT 0,
            input_tokens INTEGER NOT NULL DEFAULT 0, output_tokens INTEGER NOT NULL DEFAULT 0,
            cache_read INTEGER NOT NULL DEFAULT 0, cache_create INTEGER NOT NULL DEFAULT 0,
            cost_usd REAL NOT NULL DEFAULT 0.0, is_one_shot INTEGER NOT NULL DEFAULT 0,
            cwd TEXT);
        CREATE TABLE project_mart (
            project_id INTEGER PRIMARY KEY, provider TEXT NOT NULL, slug TEXT NOT NULL,
            display_name TEXT NOT NULL, first_ts TEXT, last_ts TEXT,
            total_messages INTEGER NOT NULL DEFAULT 0, total_sessions INTEGER NOT NULL DEFAULT 0,
            total_input_tokens INTEGER NOT NULL DEFAULT 0,
            total_output_tokens INTEGER NOT NULL DEFAULT 0,
            total_cache_read INTEGER NOT NULL DEFAULT 0,
            total_cache_create INTEGER NOT NULL DEFAULT 0,
            total_cost_usd REAL NOT NULL DEFAULT 0.0,
            total_user_messages INTEGER NOT NULL DEFAULT 0,
            total_assistant_messages INTEGER NOT NULL DEFAULT 0,
            total_tool_use_messages INTEGER NOT NULL DEFAULT 0,
            total_tool_result_messages INTEGER NOT NULL DEFAULT 0,
            total_commands INTEGER NOT NULL DEFAULT 0,
            total_records INTEGER NOT NULL DEFAULT 0,
            total_errors INTEGER NOT NULL DEFAULT 0,
            errors_by_category TEXT NOT NULL DEFAULT '{}',
            total_cache_read_messages INTEGER NOT NULL DEFAULT 0,
            total_commands_followed_by_interruption INTEGER NOT NULL DEFAULT 0,
            total_command_tools INTEGER NOT NULL DEFAULT 0,
            total_command_steps INTEGER NOT NULL DEFAULT 0);
        CREATE TABLE provider_day_mart (
            day TEXT NOT NULL, provider TEXT NOT NULL, cost_usd REAL NOT NULL DEFAULT 0.0,
            message_count INTEGER NOT NULL DEFAULT 0, session_count INTEGER NOT NULL DEFAULT 0,
            project_count INTEGER NOT NULL DEFAULT 0, PRIMARY KEY (day, provider));
        CREATE TABLE model_day_mart (
            day TEXT NOT NULL, model TEXT NOT NULL, speed TEXT NOT NULL DEFAULT 'standard',
            cost_usd REAL NOT NULL DEFAULT 0.0, input_tokens INTEGER NOT NULL DEFAULT 0,
            output_tokens INTEGER NOT NULL DEFAULT 0, cache_read INTEGER NOT NULL DEFAULT 0,
            cache_create INTEGER NOT NULL DEFAULT 0, message_count INTEGER NOT NULL DEFAULT 0,
            session_count INTEGER NOT NULL DEFAULT 0, PRIMARY KEY (day, model, speed));
        CREATE TABLE tool_mart (
            day TEXT NOT NULL, project_id INTEGER NOT NULL, provider TEXT NOT NULL,
            tool_name TEXT NOT NULL, event_count INTEGER NOT NULL DEFAULT 0,
            cost_usd REAL NOT NULL DEFAULT 0.0, tokens_in INTEGER NOT NULL DEFAULT 0,
            tokens_out INTEGER NOT NULL DEFAULT 0, session_count INTEGER NOT NULL DEFAULT 0,
            calls_total INTEGER NOT NULL DEFAULT 0, cache_read INTEGER NOT NULL DEFAULT 0,
            cache_create INTEGER NOT NULL DEFAULT 0,
            PRIMARY KEY (day, project_id, provider, tool_name));
        CREATE TABLE command_mart (
            day TEXT NOT NULL, project_id INTEGER NOT NULL, command_name TEXT NOT NULL,
            event_count INTEGER NOT NULL DEFAULT 0, cost_usd REAL NOT NULL DEFAULT 0.0,
            tokens_in INTEGER NOT NULL DEFAULT 0, tokens_out INTEGER NOT NULL DEFAULT 0,
            session_count INTEGER NOT NULL DEFAULT 0,
            PRIMARY KEY (day, project_id, command_name));
        CREATE TABLE command_day_mart (
            day TEXT NOT NULL, project_id INTEGER NOT NULL,
            command_count INTEGER NOT NULL DEFAULT 0, PRIMARY KEY (day, project_id));
        CREATE TABLE message_tool_mart (
            id INTEGER PRIMARY KEY, message_id INTEGER NOT NULL, project_id INTEGER NOT NULL,
            session_id TEXT NOT NULL, ts TEXT NOT NULL, day TEXT NOT NULL,
            tool_name TEXT NOT NULL, file_path TEXT, byte_count INTEGER,
            call_index INTEGER NOT NULL, UNIQUE (message_id, tool_name, call_index));
        CREATE TABLE mart_watermark (
            mart_name TEXT PRIMARY KEY, last_event_id INTEGER NOT NULL DEFAULT 0,
            last_refresh_ts TEXT NOT NULL);
        CREATE INDEX idx_message_tool_mart_ts ON message_tool_mart(ts, message_id, tool_name);
        CREATE INDEX idx_projects_slug ON projects(slug);
    ";

    /// An empty v030-shaped store.
    pub fn conn() -> Connection {
        let conn = Connection::open_in_memory().expect("in-memory store");
        conn.execute_batch(SCHEMA).expect("schema");
        conn
    }

    /// One `usage_events` row, with the two `GROUP BY` columns explicit.
    pub fn event(conn: &Connection, id: i64, provider: &str, cost_source: &str) {
        conn.execute(
            "INSERT INTO usage_events (id, source_message_fk, project_id, session_id,
                                       provider, model, ts, day, cost_source)
             VALUES (?, ?, 1, 's1', ?, 'm', '2026-01-01T00:00:00+00:00', '2026-01-01', ?)",
            rusqlite::params![id, id, provider, cost_source],
        )
        .expect("insert event");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `_drop_events_and_marts` must leave `mart_watermark` EMPTY. Ported as
    /// `watermark::rebuild_all_marts` instead, eight rows survive at the
    /// pre-wipe high-water mark and the `refresh_all_marts` that follows skips
    /// every event the pass just wrote. This is the trap the module docs name.
    #[test]
    fn the_force_wipe_leaves_no_watermark_behind() {
        let conn = testdb::conn();
        testdb::event(&conn, 1, "claude", "rate_card");
        conn.execute(
            "INSERT INTO daily_mart (day, project_id, provider, model, speed, cost_usd)
             VALUES ('2026-01-01', 1, 'claude', 'm', 'standard', 1.0)",
            [],
        )
        .expect("seed a mart row");
        conn.execute(
            "INSERT INTO mart_watermark (mart_name, last_event_id, last_refresh_ts)
             VALUES ('daily', 999, 't')",
            [],
        )
        .expect("seed a watermark");

        drop_events_and_marts(&conn).expect("wipe");

        let count =
            |sql: &str| -> i64 { conn.query_row(sql, [], |row| row.get(0)).expect("count") };
        assert_eq!(count("SELECT COUNT(*) FROM usage_events"), 0);
        assert_eq!(
            count("SELECT COUNT(*) FROM mart_watermark"),
            0,
            "rebuild_all_marts would have left eight rows here"
        );
        assert_eq!(count("SELECT COUNT(*) FROM daily_mart"), 0);
    }

    /// A `force=false` run over an already-converted store inserts nothing,
    /// counts every row as a skip, and still stamps all eight watermarks.
    #[test]
    fn an_incremental_backfill_is_idempotent_and_still_refreshes_every_mart() {
        let conn = testdb::conn();
        conn.execute_batch(
            "INSERT INTO projects (id, provider, slug, display_name)
                 VALUES (1, 'claude', 'p', 'p');
             INSERT INTO sessions (id, project_id, session_id) VALUES (1, 1, 's1');
             INSERT INTO messages (id, session_fk, seq, timestamp, role, model,
                                   input_tokens, output_tokens)
                 VALUES (1, 1, 0, '2026-01-01T00:00:00+00:00', 'assistant',
                         'claude-sonnet-4-5-20250929', 1000, 100);",
        )
        .expect("seed");

        let manifest = package_dir().join("data").join("models.toml");
        let Ok(ctx) = NormalizeContext::unprimed(&manifest) else {
            // The manifest lives in the Python checkout; skip rather than fail
            // when this test runs from a tree without it.
            return;
        };

        let first = backfill(&conn, &ctx, false, "2026-01-01T00:00:00+00:00").expect("first");
        assert_eq!(first.events_inserted, 1);
        assert_eq!(first.marts_refreshed.len(), 8);

        let second = backfill(&conn, &ctx, false, "2026-01-01T00:00:01+00:00").expect("second");
        assert_eq!(second.events_inserted, 0);
        assert_eq!(second.events_skipped_duplicate, 1);
        assert!(
            second.marts_refreshed.iter().all(|(_, n)| *n == 0),
            "a second refresh must not re-add the same window"
        );
        let marks: i64 = conn
            .query_row("SELECT COUNT(*) FROM mart_watermark", [], |row| row.get(0))
            .expect("count");
        assert_eq!(marks, 8);
    }

    /// `stackunderflow/` in the Python checkout this worktree carries.
    fn package_dir() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../stackunderflow")
            .canonicalize()
            .unwrap_or_else(|_| std::path::PathBuf::from("/nonexistent"))
    }
}
