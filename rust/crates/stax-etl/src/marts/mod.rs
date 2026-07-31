//! Port of `stackunderflow/etl/marts/` — all eight builders and the registry.
//!
//! Each mart is a transform from `usage_events` rows to mart rows, with an
//! independent `mart_watermark.last_event_id` so a partial failure self-heals
//! (`etl/marts/base.py`). The registry is module-level state in Python, keyed on
//! the mart name and populated at import time at the bottom of
//! `etl/marts/__init__.py`; here it is [`all`], and the order is the Python
//! registration order because `refresh_all_marts` returns its report in it.
//!
//! # SQL shapes are ported, not idiomatised
//!
//! `docs/specs/rust-port.md` §6b is explicit: `messages` is a UNION-ALL view
//! over 16 monthly partitions, SQLite does not push join predicates into the
//! arms, and the `session_fk IN (SELECT …)` list-subquery idiom is the
//! difference between 9ms and 912ms *measured*. rusqlite bundles the same
//! engine with the same planner, so every statement below is the Python string
//! with its shape intact — including the ones that look like they want a JOIN.
//!
//! # DELETE, never DROP
//!
//! `rebuild_from_scratch` clears rows with `DELETE FROM`. That is load-bearing:
//! migration v030 adds `idx_message_tool_mart_ts` and `idx_projects_slug` with
//! `CREATE INDEX IF NOT EXISTS` and nothing re-creates them, so a `DROP TABLE`
//! in a rebuild would silently cost the live tab its range seek until the next
//! migration run. `tests/stackunderflow/store/test_migration_v030.py` pins it on
//! the Python side; [`tests::rebuild_from_scratch_preserves_the_v030_indexes`]
//! pins it here.

use anyhow::Result;
use rusqlite::Connection;

pub mod command;
pub mod daily;
pub mod dims;
pub mod json;
pub mod message_tool;
pub mod model_day;
pub mod project;
pub mod provider_day;
pub mod session;
pub mod tool;
pub mod watermark;

/// `etl/marts/base.py::MartBuilder` — the incremental + full-rebuild contract.
pub trait MartBuilder {
    /// The registry key, and the `<name>_mart` table stem.
    fn name(&self) -> &'static str;

    /// Upsert mart rows for `usage_events` with `id > since_event_id`.
    ///
    /// Returns the highest `event_id` consumed; the caller persists it with
    /// [`watermark::set_watermark`]. Returning `since_event_id` means there was
    /// nothing new. Idempotent by construction — re-running a window is a no-op
    /// for rows already built.
    fn refresh(&self, conn: &Connection, since_event_id: i64) -> Result<i64>;

    /// Drop + repopulate this mart from scratch.
    ///
    /// The concrete default from `base.py`: `DELETE FROM <name>_mart` then
    /// `refresh(conn, 0)`. Every Python subclass overrides it with the identical
    /// body except `command`, which owns a second table.
    fn rebuild_from_scratch(&self, conn: &Connection) -> Result<()> {
        conn.execute(&format!("DELETE FROM {}_mart", self.name()), [])?;
        self.refresh(conn, 0)?;
        Ok(())
    }
}

/// `etl/marts/all()` — a snapshot of the registry, in registration order.
///
/// Python's `_REGISTRY` is a `dict` populated by the eight `register(...)` calls
/// at the bottom of `etl/marts/__init__.py`; `dict` preserves insertion order
/// and `refresh_all_marts` iterates it, so this order is the report order.
#[must_use]
pub fn all() -> Vec<Box<dyn MartBuilder>> {
    vec![
        Box::new(daily::DailyMartBuilder),
        Box::new(session::SessionMartBuilder),
        Box::new(project::ProjectMartBuilder),
        Box::new(provider_day::ProviderDayMartBuilder),
        Box::new(model_day::ModelDayMartBuilder),
        Box::new(tool::ToolMartBuilder),
        Box::new(command::CommandMartBuilder),
        Box::new(message_tool::MessageToolMartBuilder),
    ]
}

/// `SELECT MAX(id) FROM usage_events`, `0` when the table is empty.
///
/// Every one of the eight Python modules defines this same private helper; it
/// is one function here.
pub(crate) fn max_event_id(conn: &Connection) -> Result<i64> {
    let v: Option<i64> =
        conn.query_row("SELECT MAX(id) AS m FROM usage_events", [], |r| r.get(0))?;
    Ok(v.unwrap_or(0))
}

#[cfg(test)]
pub(crate) mod testdb {
    //! A store built from the real migrations, for the mart tests.
    //!
    //! The DDL is copied from `stackunderflow/store/migrations/` (v006, v007,
    //! v011, v012, v022, v023, v025, v030) rather than re-derived — a mart test
    //! that runs against a hand-written schema proves nothing about the mart.

    use rusqlite::Connection;

    pub const SCHEMA: &str = r"
        CREATE TABLE projects (
            id INTEGER PRIMARY KEY, provider TEXT NOT NULL, slug TEXT NOT NULL,
            display_name TEXT NOT NULL, UNIQUE (provider, slug));
        CREATE TABLE sessions (
            id INTEGER PRIMARY KEY, project_id INTEGER NOT NULL, session_id TEXT NOT NULL);
        CREATE TABLE messages (
            id INTEGER PRIMARY KEY, session_fk INTEGER NOT NULL, seq INTEGER NOT NULL,
            timestamp TEXT, role TEXT, content_text TEXT, tools_json TEXT, raw_json TEXT);
        CREATE TABLE usage_events (
            id INTEGER PRIMARY KEY, source_message_fk INTEGER, project_id INTEGER NOT NULL,
            session_id TEXT NOT NULL, provider TEXT NOT NULL, model TEXT NOT NULL DEFAULT '',
            speed TEXT NOT NULL DEFAULT 'standard', role TEXT NOT NULL DEFAULT 'assistant',
            ts TEXT NOT NULL, day TEXT NOT NULL,
            input_tokens INTEGER NOT NULL DEFAULT 0, output_tokens INTEGER NOT NULL DEFAULT 0,
            cache_read_tokens INTEGER NOT NULL DEFAULT 0,
            cache_create_tokens INTEGER NOT NULL DEFAULT 0,
            cost_usd REAL NOT NULL DEFAULT 0.0);
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

    pub fn conn() -> Connection {
        let c = Connection::open_in_memory().expect("in-memory store");
        c.execute_batch(SCHEMA).expect("schema");
        c
    }

    /// Insert a project + session pair, returning the session's rowid.
    pub fn project(c: &Connection, id: i64, slug: &str, provider: &str) {
        c.execute(
            "INSERT INTO projects (id, provider, slug, display_name) VALUES (?, ?, ?, ?)",
            rusqlite::params![id, provider, slug, slug],
        )
        .unwrap();
    }

    pub fn session(c: &Connection, fk: i64, project_id: i64, session_id: &str) {
        c.execute(
            "INSERT INTO sessions (id, project_id, session_id) VALUES (?, ?, ?)",
            rusqlite::params![fk, project_id, session_id],
        )
        .unwrap();
    }

    #[allow(clippy::too_many_arguments)]
    pub fn message(
        c: &Connection,
        id: i64,
        session_fk: i64,
        seq: i64,
        ts: &str,
        role: &str,
        content_text: &str,
        tools_json: &str,
        raw_json: &str,
    ) {
        c.execute(
            "INSERT INTO messages (id, session_fk, seq, timestamp, role, content_text, \
             tools_json, raw_json) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
            rusqlite::params![
                id,
                session_fk,
                seq,
                ts,
                role,
                content_text,
                tools_json,
                raw_json
            ],
        )
        .unwrap();
    }

    #[allow(clippy::too_many_arguments)]
    pub fn event(
        c: &Connection,
        id: i64,
        msg_fk: Option<i64>,
        project_id: i64,
        session_id: &str,
        provider: &str,
        model: &str,
        day: &str,
        tokens: (i64, i64, i64, i64),
        cost: f64,
    ) {
        c.execute(
            "INSERT INTO usage_events (id, source_message_fk, project_id, session_id, provider, \
             model, speed, role, ts, day, input_tokens, output_tokens, cache_read_tokens, \
             cache_create_tokens, cost_usd) \
             VALUES (?, ?, ?, ?, ?, ?, 'standard', 'assistant', ?, ?, ?, ?, ?, ?, ?)",
            rusqlite::params![
                id,
                msg_fk,
                project_id,
                session_id,
                provider,
                model,
                format!("{day}T00:00:00Z"),
                day,
                tokens.0,
                tokens.1,
                tokens.2,
                tokens.3,
                cost
            ],
        )
        .unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_registry_is_in_pythons_registration_order() {
        let names: Vec<&str> = all().iter().map(|m| m.name()).collect();
        assert_eq!(
            names,
            [
                "daily",
                "session",
                "project",
                "provider_day",
                "model_day",
                "tool",
                "command",
                "message_tool"
            ]
        );
    }

    #[test]
    fn rebuild_from_scratch_preserves_the_v030_indexes() {
        // v030's two indexes are `CREATE INDEX IF NOT EXISTS` in a migration
        // that has already run; nothing re-creates them. A `DROP TABLE` in a
        // rebuild would take `idx_message_tool_mart_ts` with it and the live
        // tab would silently full-scan the mart on every poll.
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
            (1, 1, 0, 0),
            1.0,
        );

        for mart in all() {
            mart.rebuild_from_scratch(&c).unwrap();
        }

        let mut stmt = c
            .prepare("SELECT name FROM sqlite_master WHERE type='index' AND name IN (?, ?)")
            .unwrap();
        let found: Vec<String> = stmt
            .query_map(["idx_message_tool_mart_ts", "idx_projects_slug"], |r| {
                r.get(0)
            })
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(found.len(), 2, "both v030 indexes must survive: {found:?}");

        // And the tables themselves are still tables, not dropped-and-gone.
        for t in [
            "daily_mart",
            "session_mart",
            "project_mart",
            "provider_day_mart",
            "model_day_mart",
            "tool_mart",
            "command_mart",
            "command_day_mart",
            "message_tool_mart",
        ] {
            let n: i64 = c
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?",
                    [t],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(n, 1, "{t} must still exist");
        }
    }
}
