//! `python-legacy: store/schema.py` — the migration runner.
//!
//! Twenty-seven `.sql` files and two `.py` data migrations reach schema **v30**.
//! [`apply`] reproduces `schema.apply(conn)` statement for statement, because the
//! two implementations write the *same* `store.db` (spec §2.1): a runner that
//! reaches the right `user_version` by a different route leaves a different
//! `sqlite_master`, and `sqlite_master` order is what `.schema` prints and what
//! `rust/schema-differ.sh` compares.
//!
//! # The SQL is not transcribed — it is the same file
//!
//! Every `.sql` body is pulled in with [`include_str!`] straight out of
//! `stackunderflow/store/migrations/`, the precedent being
//! `stax_etl::stats::dataset`'s `models.toml`. This is deliberate and it is the
//! strongest guarantee available: there is no Rust copy of the DDL to drift, and
//! a migration edited in Python that this crate has not been rebuilt against is
//! a *compile* failure, not a silent divergence. Only the two `.py` migrations
//! are real ports ([`v005`], [`v008`]) — they read rows before rewriting them,
//! which no `.sql` file can do.
//!
//! # The four rules the runner itself has to get right
//!
//! 1. **`current` is read once.** Python reads `PRAGMA user_version` before the
//!    loop and compares every migration against that one value; the versions the
//!    migrations set as they run do not re-arm the comparison. Re-reading per
//!    iteration would be equivalent *today* and would stop being equivalent the
//!    first time a migration failed to bump.
//! 2. **A guard hit bumps and skips.** `_ADD_COLUMN_GUARDS` maps a version to
//!    `(table, column)`; when that column already exists the body is *not* run
//!    and `user_version` is set anyway, so the chain continues. This is the
//!    "operator pre-ran the ALTER" / "crashed after the DDL, before the bump"
//!    recovery path, and it is the half of the runner a from-empty differ can
//!    never exercise — hence the differ's mid-version states.
//! 3. **`.sql` files own their own transaction.** All twenty-seven wrap
//!    themselves in `BEGIN; … PRAGMA user_version = N; COMMIT;`, so the runner
//!    hands the whole file to `execute_batch` (sqlite3_exec — what
//!    `executescript` is) and never sets the version itself.
//! 4. **`.py` migrations do NOT own theirs.** The runner wraps them:
//!    `BEGIN` / body / `PRAGMA user_version = N` / `COMMIT`, with `ROLLBACK` on
//!    any error — which is why `v008` uses per-statement `execute` and not
//!    `executescript` (that would implicitly commit and break the rollback
//!    contract). Reproduced exactly.
//!
//! # Why `v005` takes an injected callback
//!
//! `v005` replays the **cursor adapter's** workspace-slug rule against persisted
//! `raw_json`. Python late-imports the adapter from the store layer and says in
//! its own comment why: importing at module load would create a hard dependency
//! from the store onto an adapter. Rust has no late import, and the cost of the
//! hard edge is not hypothetical — `stax-core` is under `stax-hooks`, whose
//! whole budget is a 2 ms spawn. So the rule is [`Hooks::cursor_slug`], injected
//! by a caller that already links `stax-adapters`. With no hook the sessions are
//! *unresolved*, which is `v005`'s own no-path-evidence branch (the legacy row is
//! kept and renamed) — **DIV-301**, reachable only on a store that predates
//! v0.6.1 and is still below v5.

use rusqlite::Connection;

mod v005;
mod v008;

/// `schema.CURRENT_VERSION` — the version a fully migrated store reports.
pub const CURRENT_VERSION: i64 = 30;

/// What a migration *is*: a file of DDL, or Rust that reads rows first.
enum Body {
    /// A `.sql` file, verbatim. Sets its own `PRAGMA user_version`.
    Sql(&'static str),
    /// A ported `.py` migration. The runner owns its transaction and its bump.
    Rust(fn(&Connection, &Hooks<'_>) -> rusqlite::Result<()>),
}

/// One row of `_discover()`.
struct Migration {
    version: i64,
    /// The file's stem, for error messages and for the differ's log.
    name: &'static str,
    body: Body,
}

/// `v005`'s adapter rule: every non-empty `raw_json` of one session in, the
/// workspace slug the cursor adapter would derive out.
///
/// A named alias rather than an inline `&dyn Fn(…)` because the inline form is
/// `clippy::type_complexity`, and because the signature IS the contract between
/// the store layer and whichever crate supplies the rule.
pub type CursorSlugRule<'a> = &'a dyn Fn(&[String]) -> Option<String>;

/// The adapter rules the store layer refuses to depend on directly.
///
/// Default is "no rule available", which is not the same as "the rule said no":
/// see the module doc and DIV-301.
#[derive(Default, Clone, Copy)]
pub struct Hooks<'a> {
    /// `v005`'s `_derive_slug_for_session`, minus the SQL.
    ///
    /// Receives every non-empty `messages.raw_json` for one session, in
    /// `messages` order, and returns the workspace slug the cursor adapter would
    /// derive — `None` when the payloads carry no absolute-path evidence, which
    /// is exactly Python's `None` return.
    pub cursor_slug: Option<CursorSlugRule<'a>>,
}

impl std::fmt::Debug for Hooks<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Hooks")
            .field("cursor_slug", &self.cursor_slug.map(|_| "<fn>"))
            .finish()
    }
}

/// `_discover()`, resolved at compile time.
///
/// Python sorts the directory listing, keeps `vNNN` stems with a `.sql` or `.py`
/// suffix, and sorts by the number. The result is this list, in this order —
/// note that **v015 does not exist** (the number was skipped upstream), which is
/// why the ordering rule is "sort by the parsed number" and not "count from 1".
const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        name: "v001_initial",
        body: Body::Sql(include_str!("../../../assets/migrations/v001_initial.sql")),
    },
    Migration {
        version: 2,
        name: "v002_ingest_log_multistore",
        body: Body::Sql(include_str!(
            "../../../assets/migrations/v002_ingest_log_multistore.sql"
        )),
    },
    Migration {
        version: 3,
        name: "v003_messages_speed",
        body: Body::Sql(include_str!(
            "../../../assets/migrations/v003_messages_speed.sql"
        )),
    },
    Migration {
        version: 4,
        name: "v004_clean_synthetic_models",
        body: Body::Sql(include_str!(
            "../../../assets/migrations/v004_clean_synthetic_models.sql"
        )),
    },
    Migration {
        version: 5,
        name: "v005_cursor_workspace_redistribute",
        body: Body::Rust(v005::apply),
    },
    Migration {
        version: 6,
        name: "v006_etl_layer",
        body: Body::Sql(include_str!(
            "../../../assets/migrations/v006_etl_layer.sql"
        )),
    },
    Migration {
        version: 7,
        name: "v007_lower_grain_marts",
        body: Body::Sql(include_str!(
            "../../../assets/migrations/v007_lower_grain_marts.sql"
        )),
    },
    Migration {
        version: 8,
        name: "v008_messages_partitioning",
        body: Body::Rust(v008::apply),
    },
    Migration {
        version: 9,
        name: "v009_discovery_telemetry",
        body: Body::Sql(include_str!(
            "../../../assets/migrations/v009_discovery_telemetry.sql"
        )),
    },
    Migration {
        version: 10,
        name: "v010_captured_events",
        body: Body::Sql(include_str!(
            "../../../assets/migrations/v010_captured_events.sql"
        )),
    },
    Migration {
        version: 11,
        name: "v011_message_tool_mart",
        body: Body::Sql(include_str!(
            "../../../assets/migrations/v011_message_tool_mart.sql"
        )),
    },
    Migration {
        version: 12,
        name: "v012_tool_mart_calls_total",
        body: Body::Sql(include_str!(
            "../../../assets/migrations/v012_tool_mart_calls_total.sql"
        )),
    },
    Migration {
        version: 13,
        name: "v013_multi_agent_session_metadata",
        body: Body::Sql(include_str!(
            "../../../assets/migrations/v013_multi_agent_session_metadata.sql"
        )),
    },
    Migration {
        version: 14,
        name: "v014_discovery_embeddings",
        body: Body::Sql(include_str!(
            "../../../assets/migrations/v014_discovery_embeddings.sql"
        )),
    },
    Migration {
        version: 16,
        name: "v016_mode_recommendations",
        body: Body::Sql(include_str!(
            "../../../assets/migrations/v016_mode_recommendations.sql"
        )),
    },
    Migration {
        version: 17,
        name: "v017_pr_ci_outcomes",
        body: Body::Sql(include_str!(
            "../../../assets/migrations/v017_pr_ci_outcomes.sql"
        )),
    },
    Migration {
        version: 18,
        name: "v018_static_analysis_findings",
        body: Body::Sql(include_str!(
            "../../../assets/migrations/v018_static_analysis_findings.sql"
        )),
    },
    Migration {
        version: 19,
        name: "v019_commit_session_link",
        body: Body::Sql(include_str!(
            "../../../assets/migrations/v019_commit_session_link.sql"
        )),
    },
    Migration {
        version: 20,
        name: "v020_session_quality_metrics",
        body: Body::Sql(include_str!(
            "../../../assets/migrations/v020_session_quality_metrics.sql"
        )),
    },
    Migration {
        version: 21,
        name: "v021_grade_no_fabricated_fallback",
        body: Body::Sql(include_str!(
            "../../../assets/migrations/v021_grade_no_fabricated_fallback.sql"
        )),
    },
    Migration {
        version: 22,
        name: "v022_project_mart_message_dims",
        body: Body::Sql(include_str!(
            "../../../assets/migrations/v022_project_mart_message_dims.sql"
        )),
    },
    Migration {
        version: 23,
        name: "v023_mart_overview_rate_dims",
        body: Body::Sql(include_str!(
            "../../../assets/migrations/v023_mart_overview_rate_dims.sql"
        )),
    },
    Migration {
        version: 24,
        name: "v024_price_book",
        body: Body::Sql(include_str!(
            "../../../assets/migrations/v024_price_book.sql"
        )),
    },
    Migration {
        version: 25,
        name: "v025_command_day_mart",
        body: Body::Sql(include_str!(
            "../../../assets/migrations/v025_command_day_mart.sql"
        )),
    },
    Migration {
        version: 26,
        name: "v026_reasoning_tokens",
        body: Body::Sql(include_str!(
            "../../../assets/migrations/v026_reasoning_tokens.sql"
        )),
    },
    Migration {
        version: 27,
        name: "v027_worktree_of",
        body: Body::Sql(include_str!(
            "../../../assets/migrations/v027_worktree_of.sql"
        )),
    },
    Migration {
        version: 28,
        name: "v028_sync_identity_outbox",
        body: Body::Sql(include_str!(
            "../../../assets/migrations/v028_sync_identity_outbox.sql"
        )),
    },
    Migration {
        version: 29,
        name: "v029_sync_pull_landing",
        body: Body::Sql(include_str!(
            "../../../assets/migrations/v029_sync_pull_landing.sql"
        )),
    },
    Migration {
        version: 30,
        name: "v030_live_indexes",
        body: Body::Sql(include_str!(
            "../../../assets/migrations/v030_live_indexes.sql"
        )),
    },
];

/// `_ADD_COLUMN_GUARDS` — `version → (table, column)`.
///
/// Only migrations whose body is `ALTER TABLE … ADD COLUMN` (or a
/// `CREATE TABLE IF NOT EXISTS` whose presence a column can prove) get an entry.
/// **v030 is deliberately absent**: it is two `CREATE INDEX IF NOT EXISTS`
/// statements, and a guard would need a table/column pair that says nothing
/// about whether the indexes exist. Copied with the reasoning intact because a
/// guard added here that Python does not have silently *skips* a migration body.
const ADD_COLUMN_GUARDS: &[(i64, &str, &str)] = &[
    (3, "messages", "speed"),
    (12, "tool_mart", "calls_total"),
    (13, "sessions", "team_id"),
    (22, "project_mart", "total_user_messages"),
    (23, "project_mart", "total_records"),
    (24, "price_book", "model"),
    (25, "command_day_mart", "command_count"),
    (26, "usage_events", "reasoning_tokens"),
    (27, "projects", "worktree_of"),
    (28, "sync_identity", "device_uuid"),
    (29, "sync_cursors", "remote_device_uuid"),
];

/// `_discover()`'s result, as `(version, file stem)` pairs in run order.
///
/// The runner does not need this — [`MIGRATIONS`] is a `const` — but a
/// *diagnostic* does: `stax-schema-apply --list` prints it, and the differ logs
/// it, so "which migrations does this binary believe in" is answerable without
/// reading the source. It is also the assertion target for the test that the
/// embedded set matches the reference directory listing.
#[must_use]
pub fn manifest() -> Vec<(i64, &'static str)> {
    MIGRATIONS
        .iter()
        .map(|migration| (migration.version, migration.name))
        .collect()
}

/// Run every pending migration against `conn`.
///
/// The no-hook entry point: `v005`'s cursor rule is unavailable, which is the
/// DIV-301 leg. Every caller that reaches a store built after v0.6.1 — which is
/// every caller the campaign has — is unaffected, because `v005` is a no-op
/// without a legacy `(cursor, cursor)` project row.
///
/// # Errors
/// Any SQLite failure, or `v008`'s row-loss check.
pub fn apply(conn: &Connection) -> rusqlite::Result<()> {
    apply_with(conn, &Hooks::default())
}

/// [`apply`], with the adapter rules injected.
///
/// # Errors
/// Any SQLite failure, or `v008`'s row-loss check.
pub fn apply_with(conn: &Connection, hooks: &Hooks<'_>) -> rusqlite::Result<()> {
    apply_upto(conn, CURRENT_VERSION, hooks)
}

/// [`apply_with`], stopping after migration `target`.
///
/// **Not a reference surface** — `schema.py` has no such entry point. It exists
/// because a runner can only be *proven* against mid-version states, and both
/// implementations have to be able to build one: `rust/schema-differ.sh` walks
/// every stop point, migrates to it with one implementation, and finishes with
/// the other. Everything else about the loop, including the guards, is
/// identical to [`apply_with`]'s.
///
/// # Errors
/// Any SQLite failure, or `v008`'s row-loss check.
pub fn apply_upto(conn: &Connection, target: i64, hooks: &Hooks<'_>) -> rusqlite::Result<()> {
    let current: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    for migration in MIGRATIONS {
        if migration.version <= current || migration.version > target {
            continue;
        }
        if let Some(guard) = guard_for(migration.version)
            && column_exists(conn, guard.0, guard.1)?
        {
            set_user_version(conn, migration.version)?;
            continue;
        }
        match migration.body {
            Body::Sql(sql) => conn.execute_batch(sql)?,
            Body::Rust(run) => run_data_migration(conn, migration, run, hooks)?,
        }
    }
    Ok(())
}

/// `_run_python_migration` — the body and the bump share one transaction.
fn run_data_migration(
    conn: &Connection,
    migration: &Migration,
    run: fn(&Connection, &Hooks<'_>) -> rusqlite::Result<()>,
    hooks: &Hooks<'_>,
) -> rusqlite::Result<()> {
    conn.execute_batch("BEGIN")?;
    match run(conn, hooks).and_then(|()| set_user_version(conn, migration.version)) {
        Ok(()) => conn.execute_batch("COMMIT"),
        Err(err) => {
            // Python's `except: ROLLBACK; raise` — the rollback's own failure
            // must not replace the error that caused it.
            let _ = conn.execute_batch("ROLLBACK");
            Err(err)
        }
    }
}

fn guard_for(version: i64) -> Option<(&'static str, &'static str)> {
    ADD_COLUMN_GUARDS
        .iter()
        .find(|(guarded, _, _)| *guarded == version)
        .map(|(_, table, column)| (*table, *column))
}

/// `_column_exists` — `PRAGMA table_info(<table>)`, looking for `<column>`.
///
/// Python interpolates the table name into the pragma and so does this; the
/// names are compile-time constants in both. A pragma against a table that does
/// not exist returns **no rows** rather than raising, which is the property that
/// lets the same helper double as "does this table exist at all?" for the four
/// `CREATE TABLE IF NOT EXISTS` guards (v024/v025/v028/v029).
fn column_exists(conn: &Connection, table: &str, column: &str) -> rusqlite::Result<bool> {
    let mut statement = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let mut rows = statement.query([])?;
    while let Some(row) = rows.next()? {
        let name: String = row.get(1)?;
        if name == column {
            return Ok(true);
        }
    }
    Ok(false)
}

/// `PRAGMA user_version = N`. Not parameterisable — pragmas never are.
fn set_user_version(conn: &Connection, version: i64) -> rusqlite::Result<()> {
    conn.execute_batch(&format!("PRAGMA user_version = {version}"))
}

/// The error shape a ported data migration raises for a non-SQLite reason.
///
/// `v008` raises `RuntimeError` when the partition copy loses rows. There is no
/// free-form variant in `rusqlite::Error`, and inventing one would change every
/// caller's signature, so it goes out as a `SqliteFailure` carrying the message
/// — `Display` prints the message, which is what the caller shows a user.
pub(crate) fn migration_error(message: String) -> rusqlite::Error {
    rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_ERROR),
        Some(message),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh() -> Connection {
        let conn = Connection::open_in_memory().expect("in-memory store");
        conn.execute_batch("PRAGMA foreign_keys = ON")
            .expect("pragma");
        conn
    }

    fn user_version(conn: &Connection) -> i64 {
        conn.query_row("PRAGMA user_version", [], |row| row.get(0))
            .expect("user_version")
    }

    #[test]
    fn the_migration_list_is_ordered_and_skips_fifteen() {
        let versions: Vec<i64> = MIGRATIONS.iter().map(|m| m.version).collect();
        assert!(
            versions.windows(2).all(|pair| pair[0] < pair[1]),
            "_discover() sorts by the parsed number: {versions:?}"
        );
        assert!(
            !versions.contains(&15),
            "there is no v015 migration in the reference tree"
        );
        assert_eq!(versions.last().copied(), Some(CURRENT_VERSION));
        assert_eq!(versions.len(), 29);
    }

    #[test]
    fn the_manifest_names_the_reference_files() {
        // Each stem is `v{version:03}_…`, which is what `_discover()` parses the
        // number back out of — a mismatch here means an `include_str!` points at
        // a different file than the entry claims.
        for (version, name) in manifest() {
            let prefix = format!("v{version:03}_");
            assert!(
                name.starts_with(&prefix),
                "{name} is filed under version {version}"
            );
        }
        assert_eq!(manifest().len(), MIGRATIONS.len());
    }

    #[test]
    fn every_sql_body_sets_its_own_user_version() {
        // The runner never bumps for a `.sql` file, so a file that forgot the
        // pragma would leave the chain stuck one version short — silently, and
        // only on a store that had not already passed that version.
        for migration in MIGRATIONS {
            if let Body::Sql(sql) = migration.body {
                let needle = format!("PRAGMA user_version = {}", migration.version);
                assert!(
                    sql.contains(&needle),
                    "{} does not contain `{needle}`",
                    migration.name
                );
            }
        }
    }

    #[test]
    fn from_empty_reaches_the_current_version() {
        let conn = fresh();
        apply(&conn).expect("apply from empty");
        assert_eq!(user_version(&conn), CURRENT_VERSION);
    }

    #[test]
    fn apply_is_idempotent() {
        let conn = fresh();
        apply(&conn).expect("first");
        let before = schema_dump(&conn);
        apply(&conn).expect("second");
        apply(&conn).expect("third");
        assert_eq!(user_version(&conn), CURRENT_VERSION);
        assert_eq!(
            before,
            schema_dump(&conn),
            "a second apply changed the schema"
        );
    }

    fn schema_dump(conn: &Connection) -> Vec<String> {
        let mut statement = conn
            .prepare(
                "SELECT type, name, COALESCE(sql, '') FROM sqlite_master \
                 WHERE name NOT LIKE 'sqlite_%' ORDER BY rowid",
            )
            .expect("prepare");
        let rows = statement
            .query_map([], |row| {
                Ok(format!(
                    "{}|{}|{}",
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?
                ))
            })
            .expect("query");
        rows.map(|row| row.expect("row")).collect()
    }

    #[test]
    fn the_store_a_fresh_apply_builds_carries_the_sync_tables() {
        // DIV-216's actual subject: `stax sync` needs v028 + v029 present.
        let conn = fresh();
        apply(&conn).expect("apply");
        for table in [
            "sync_identity",
            "sync_outbox",
            "sync_cursors",
            "sync_remote_devices",
        ] {
            let found: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
                    [table],
                    |row| row.get(0),
                )
                .expect("count");
            assert_eq!(found, 1, "{table} is missing after a full apply");
        }
    }

    #[test]
    fn messages_ends_up_a_view_over_partitions() {
        let conn = fresh();
        apply(&conn).expect("apply");
        let kind: String = conn
            .query_row(
                "SELECT type FROM sqlite_master WHERE name = 'messages'",
                [],
                |row| row.get(0),
            )
            .expect("messages");
        assert_eq!(kind, "view", "v008 leaves `messages` a view");
        let unknown: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master \
                 WHERE type = 'table' AND name = 'messages_unknown'",
                [],
                |row| row.get(0),
            )
            .expect("count");
        assert_eq!(unknown, 1, "the fallback partition is always created");
    }

    #[test]
    fn a_guard_hit_bumps_the_version_without_running_the_body() {
        // Stop at v2, hand-apply v003's ALTER, leave user_version behind — the
        // "crashed after the DDL" state. Re-running must NOT error on a
        // duplicate column, and must not stall at 2.
        let conn = fresh();
        apply_to(&conn, 2);
        assert_eq!(user_version(&conn), 2);
        conn.execute_batch(
            "ALTER TABLE messages ADD COLUMN speed TEXT NOT NULL DEFAULT 'standard'",
        )
        .expect("hand-applied ALTER");
        assert_eq!(
            user_version(&conn),
            2,
            "the hand-apply left the version behind"
        );
        apply(&conn).expect("the guard recovers the partial application");
        assert_eq!(user_version(&conn), CURRENT_VERSION);
    }

    #[test]
    fn a_guard_is_only_consulted_for_its_own_version() {
        assert_eq!(guard_for(3), Some(("messages", "speed")));
        assert_eq!(
            guard_for(30),
            None,
            "v030 is index-only and must stay unguarded"
        );
        assert_eq!(guard_for(1), None);
    }

    #[test]
    fn column_exists_answers_false_for_a_missing_table() {
        let conn = fresh();
        assert!(!column_exists(&conn, "nope", "whatever").expect("pragma"));
    }

    /// Run migrations up to and including `target`, the way the differ's
    /// mid-version states are built.
    pub(super) fn apply_to(conn: &Connection, target: i64) {
        apply_upto(conn, target, &Hooks::default()).expect("partial apply");
    }

    #[test]
    fn every_intermediate_version_reaches_thirty() {
        // The mid-version half of the differ, in-process: stopping anywhere and
        // resuming must land on the same schema as going straight through.
        let straight = fresh();
        apply(&straight).expect("straight through");
        let reference = schema_dump(&straight);

        for stop in [1, 2, 4, 7, 8, 14, 23, 27, 29] {
            let conn = fresh();
            apply_to(&conn, stop);
            assert_eq!(user_version(&conn), stop, "stopping at {stop}");
            apply(&conn).expect("resume");
            assert_eq!(user_version(&conn), CURRENT_VERSION, "resuming from {stop}");
            assert_eq!(
                schema_dump(&conn),
                reference,
                "resuming from v{stop} built a different schema"
            );
        }
    }
}
