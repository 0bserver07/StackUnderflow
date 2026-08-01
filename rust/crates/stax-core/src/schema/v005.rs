//! `v005_cursor_workspace_redistribute.py` — split the collapsed `cursor`
//! project back into one project per workspace.
//!
//! Before v0.6.1 the cursor adapter stamped every conversation with
//! `project_slug = "cursor"`, collapsing every workspace into one row. The
//! adapter now derives a real slug from the absolute paths a conversation
//! references, but `ingest_log` keys cursor entries on `(file_path, session_id)`
//! — an unchanged `.vscdb` is skipped, so without this migration the legacy row
//! lingers forever.
//!
//! # What is ported and what is injected
//!
//! Everything with a `conn` in it is here. The slug *rule* is not: it is the
//! cursor adapter's, Python late-imports it precisely so the store layer does
//! not depend on an adapter, and this port keeps that edge by taking
//! [`Hooks::cursor_slug`]. With no hook every session is **unresolved**, which
//! is the same branch Python takes for a session with no path evidence: the
//! legacy row survives with its `display_name` flagged. Recorded as **DIV-301**
//! rather than papered over, and reachable only on a store that both predates
//! v0.6.1 *and* is still below v5 — the campaign's live store passed v5 long ago
//! and re-running is a no-op by construction.
//!
//! # Idempotence
//!
//! The first statement asks for `(provider='cursor', slug='cursor')`. After a
//! successful pass either the row is gone (everything moved) or every session
//! under it is unresolvable — so a second run is a no-op in the first case and
//! reaches the identical end state in the second.

use rusqlite::{Connection, OptionalExtension as _};

use super::Hooks;

const LEGACY_SLUG: &str = "cursor";
const LEGACY_DISPLAY_NAME: &str = "cursor (legacy — reingest to split by workspace)";

/// Run the redistribute pass.
///
/// Called inside the transaction [`super::run_data_migration`] opens, so any
/// error leaves the store on the previous `user_version`.
pub(super) fn apply(conn: &Connection, hooks: &Hooks<'_>) -> rusqlite::Result<()> {
    // `.optional()` and not `.ok()`: "no such row" is the early return, a broken
    // store is an error Python would have raised too.
    let legacy = conn
        .query_row(
            "SELECT id FROM projects WHERE provider = 'cursor' AND slug = ?",
            [LEGACY_SLUG],
            |row| row.get::<_, i64>(0),
        )
        .optional()?;
    // No legacy collapse to fix — fresh store, or already migrated.
    let Some(legacy_id) = legacy else {
        return Ok(());
    };

    let sessions = {
        let mut statement =
            conn.prepare("SELECT id, session_id FROM sessions WHERE project_id = ?")?;
        let rows = statement.query_map([legacy_id], |row| row.get::<_, i64>(0))?;
        rows.collect::<rusqlite::Result<Vec<i64>>>()?
    };

    for session_pk in sessions {
        let slug = derive_slug_for_session(conn, session_pk, hooks)?;
        let Some(slug) = slug.filter(|slug| slug != LEGACY_SLUG) else {
            continue;
        };
        let target_id = ensure_project(conn, &slug)?;
        conn.execute(
            "UPDATE sessions SET project_id = ? WHERE id = ?",
            (target_id, session_pk),
        )?;
    }

    let remaining: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sessions WHERE project_id = ?",
        [legacy_id],
        |row| row.get(0),
    )?;
    if remaining == 0 {
        conn.execute("DELETE FROM projects WHERE id = ?", [legacy_id])?;
    } else {
        conn.execute(
            "UPDATE projects SET display_name = ? WHERE id = ?",
            rusqlite::params![LEGACY_DISPLAY_NAME, legacy_id],
        )?;
    }
    Ok(())
}

/// `_derive_slug_for_session` — the SQL half; the rule half is the hook.
///
/// Python skips a falsy `raw_json` (`NULL` *and* `''`) before parsing, so the
/// hook never sees one; everything after that — JSON parse failures, non-dict
/// payloads, the path sweep — is the adapter's, and lives behind the hook.
fn derive_slug_for_session(
    conn: &Connection,
    session_fk: i64,
    hooks: &Hooks<'_>,
) -> rusqlite::Result<Option<String>> {
    let Some(rule) = hooks.cursor_slug else {
        return Ok(None);
    };
    let mut statement = conn.prepare("SELECT raw_json FROM messages WHERE session_fk = ?")?;
    let rows = statement.query_map([session_fk], |row| row.get::<_, Option<String>>(0))?;
    let mut payloads = Vec::new();
    for row in rows {
        if let Some(raw) = row?
            && !raw.is_empty()
        {
            payloads.push(raw);
        }
    }
    Ok(rule(&payloads))
}

/// `_ensure_project` — the `(provider='cursor', slug=…)` row, created if absent.
///
/// The new row borrows `first_seen` / `last_modified` from the legacy row so it
/// has a plausible recency signal rather than 0; `path` is NULL and
/// `display_name` is the slug, exactly as the reference inserts them.
fn ensure_project(conn: &Connection, slug: &str) -> rusqlite::Result<i64> {
    let existing = conn
        .query_row(
            "SELECT id FROM projects WHERE provider = 'cursor' AND slug = ?",
            [slug],
            |row| row.get::<_, i64>(0),
        )
        .optional()?;
    if let Some(id) = existing {
        return Ok(id);
    }

    let (first_seen, last_modified) = conn
        .query_row(
            "SELECT first_seen, last_modified FROM projects \
             WHERE provider = 'cursor' AND slug = ?",
            [LEGACY_SLUG],
            |row| Ok((row.get::<_, f64>(0)?, row.get::<_, f64>(1)?)),
        )
        .optional()?
        .unwrap_or((0.0, 0.0));

    conn.execute(
        "INSERT INTO projects \
         (provider, slug, path, display_name, first_seen, last_modified) \
         VALUES ('cursor', ?, ?, ?, ?, ?)",
        rusqlite::params![slug, None::<String>, slug, first_seen, last_modified],
    )?;
    Ok(conn.last_insert_rowid())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A store at v4 — the state v005 actually runs against.
    fn store_at_v4() -> Connection {
        let conn = Connection::open_in_memory().expect("in-memory store");
        crate::schema::tests::apply_to(&conn, 4);
        conn
    }

    fn seed_legacy(conn: &Connection) -> i64 {
        conn.execute(
            "INSERT INTO projects (provider, slug, path, display_name, first_seen, last_modified) \
             VALUES ('cursor', 'cursor', NULL, 'cursor', 100.0, 200.0)",
            [],
        )
        .expect("legacy project");
        conn.last_insert_rowid()
    }

    fn seed_session(conn: &Connection, project_id: i64, session_id: &str, raw: &str) -> i64 {
        conn.execute(
            "INSERT INTO sessions (project_id, session_id) VALUES (?, ?)",
            rusqlite::params![project_id, session_id],
        )
        .expect("session");
        let session_pk = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO messages (session_fk, seq, timestamp, role, raw_json) \
             VALUES (?, 0, '2026-01-01T00:00:00Z', 'user', ?)",
            rusqlite::params![session_pk, raw],
        )
        .expect("message");
        session_pk
    }

    #[test]
    fn no_legacy_row_is_a_no_op() {
        let conn = store_at_v4();
        apply(&conn, &Hooks::default()).expect("apply");
        let projects: i64 = conn
            .query_row("SELECT COUNT(*) FROM projects", [], |row| row.get(0))
            .expect("count");
        assert_eq!(projects, 0);
    }

    #[test]
    fn a_resolvable_session_is_reparented_and_the_legacy_row_dropped() {
        let conn = store_at_v4();
        let legacy = seed_legacy(&conn);
        seed_session(&conn, legacy, "s1", r#"{"any":"payload"}"#);

        let rule = |payloads: &[String]| -> Option<String> {
            assert_eq!(payloads.len(), 1);
            Some("-home-me-proj".to_owned())
        };
        let hooks = Hooks {
            cursor_slug: Some(&rule),
        };
        apply(&conn, &hooks).expect("apply");

        let (slug, path_is_null, display, first_seen): (String, bool, String, f64) = conn
            .query_row(
                "SELECT slug, path IS NULL, display_name, first_seen FROM projects",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .expect("the new project row");
        assert_eq!(slug, "-home-me-proj");
        assert!(path_is_null, "the reference inserts a NULL path");
        assert_eq!(display, "-home-me-proj");
        assert!(
            (first_seen - 100.0).abs() < f64::EPSILON,
            "timestamps are borrowed from the legacy row"
        );

        let legacy_left: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM projects WHERE slug = 'cursor'",
                [],
                |row| row.get(0),
            )
            .expect("count");
        assert_eq!(legacy_left, 0, "an emptied legacy row is deleted");
    }

    #[test]
    fn an_unresolvable_session_keeps_and_flags_the_legacy_row() {
        let conn = store_at_v4();
        let legacy = seed_legacy(&conn);
        seed_session(&conn, legacy, "s1", r#"{"any":"payload"}"#);

        // No hook: DIV-301's leg, and also Python's own no-evidence branch.
        apply(&conn, &Hooks::default()).expect("apply");

        let display: String = conn
            .query_row(
                "SELECT display_name FROM projects WHERE slug = 'cursor'",
                [],
                |row| row.get(0),
            )
            .expect("the legacy row survives");
        assert_eq!(display, LEGACY_DISPLAY_NAME);
    }

    #[test]
    fn a_rule_that_returns_the_legacy_slug_counts_as_unresolved() {
        let conn = store_at_v4();
        let legacy = seed_legacy(&conn);
        seed_session(&conn, legacy, "s1", r#"{"any":"payload"}"#);

        let rule = |_: &[String]| Some("cursor".to_owned());
        let hooks = Hooks {
            cursor_slug: Some(&rule),
        };
        apply(&conn, &hooks).expect("apply");

        let display: String = conn
            .query_row(
                "SELECT display_name FROM projects WHERE slug = 'cursor'",
                [],
                |row| row.get(0),
            )
            .expect("the legacy row survives");
        assert_eq!(
            display, LEGACY_DISPLAY_NAME,
            "`slug == LEGACY_SLUG` is the second half of Python's guard"
        );
    }

    #[test]
    fn two_sessions_in_one_workspace_share_one_new_project_row() {
        let conn = store_at_v4();
        let legacy = seed_legacy(&conn);
        seed_session(&conn, legacy, "s1", r#"{"a":1}"#);
        seed_session(&conn, legacy, "s2", r#"{"a":2}"#);

        let rule = |_: &[String]| Some("-home-me-proj".to_owned());
        let hooks = Hooks {
            cursor_slug: Some(&rule),
        };
        apply(&conn, &hooks).expect("apply");

        let projects: i64 = conn
            .query_row("SELECT COUNT(*) FROM projects", [], |row| row.get(0))
            .expect("count");
        assert_eq!(projects, 1, "_ensure_project is a get-or-create");
    }

    #[test]
    fn an_empty_raw_json_never_reaches_the_rule() {
        let conn = store_at_v4();
        let legacy = seed_legacy(&conn);
        seed_session(&conn, legacy, "s1", "");

        let rule = |payloads: &[String]| -> Option<String> {
            assert!(payloads.is_empty(), "`if not raw: continue` drops '' too");
            None
        };
        let hooks = Hooks {
            cursor_slug: Some(&rule),
        };
        apply(&conn, &hooks).expect("apply");
    }
}
