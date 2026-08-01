//! `v008_messages_partitioning.py` — `messages` becomes a UNION-ALL view over
//! monthly partition tables.
//!
//! This is the single most schema-visible migration in the tree, and every byte
//! of the DDL it *generates* ends up in `sqlite_master` — the partition tables,
//! the view, and the `messages_insert_route` INSTEAD OF trigger are all built by
//! string concatenation at migration time. So the port cannot paraphrase: the
//! generated text is compared by `rust/schema-differ.sh` through `.schema`, and
//! a single extra space in the trigger body is a divergence.
//!
//! What that means concretely, and is easy to get wrong:
//!
//! * The `CREATE TABLE` bodies keep Python's **triple-quoted indentation**.
//!   SQLite stores the statement text from `CREATE` to the closing token
//!   verbatim, so the twelve-space column indent and the eight-space closing
//!   paren are part of the schema.
//! * `CREATE INDEX idx_events_day      ON …` keeps its **column-aligned runs of
//!   spaces**. Six indexes, three different run lengths.
//! * The `usage_events` rebuild ends in `ALTER TABLE … RENAME TO`, and SQLite
//!   rewrites the stored `CREATE TABLE usage_events_new (…` into
//!   `CREATE TABLE "usage_events" (…` — quotes included. Both implementations
//!   inherit that rewrite because both do the rename; neither writes it.
//!
//! # The wall clock in the schema — DIV-302
//!
//! On a store with **no messages** the month discovery finds nothing and the
//! migration bootstraps `months = [utcnow().strftime("%Y%m")]`. The name of the
//! first partition table, the view's first `SELECT`, and the trigger's month
//! list therefore depend on *when the store was created*. It is Python's
//! behaviour and it is ported as-is, but it means two stores created either side
//! of a month boundary have legitimately different schemas — so
//! `schema-differ.sh` records the UTC month before and after every run and
//! aborts on a rollover rather than reporting a divergence it caused itself.

use rusqlite::Connection;

use super::{Hooks, migration_error};

/// `_PARTITION_COLUMNS` — the shape every partition exposes, in view order.
const PARTITION_COLUMNS: &[&str] = &[
    "id",
    "session_fk",
    "seq",
    "timestamp",
    "role",
    "model",
    "input_tokens",
    "output_tokens",
    "cache_create_tokens",
    "cache_read_tokens",
    "content_text",
    "tools_json",
    "raw_json",
    "is_sidechain",
    "uuid",
    "parent_uuid",
    "speed",
];

/// `_COLUMN_DEFAULTS` — the literals the INSTEAD OF trigger has to re-apply by
/// hand, because `NEW.col` is NULL for a column the original INSERT omitted and
/// a partition's own DEFAULT only fires on a direct table insert.
const COLUMN_DEFAULTS: &[(&str, &str)] = &[
    ("input_tokens", "0"),
    ("output_tokens", "0"),
    ("cache_create_tokens", "0"),
    ("cache_read_tokens", "0"),
    ("content_text", "''"),
    ("tools_json", "'[]'"),
    ("is_sidechain", "0"),
    ("speed", "'standard'"),
];

/// The `SELECT DISTINCT` that buckets a timestamp into `YYYYMM` or `unknown`.
/// The `GLOB` legs are what make a malformed timestamp route to `unknown`
/// instead of producing a partition named after garbage.
const MONTH_DISCOVERY_SQL: &str = "SELECT DISTINCT \
       CASE \
         WHEN length(timestamp) >= 7 \
              AND substr(timestamp, 5, 1) = '-' \
              AND substr(timestamp, 1, 4) GLOB '[0-9][0-9][0-9][0-9]' \
              AND substr(timestamp, 6, 2) GLOB '[0-9][0-9]' \
         THEN substr(timestamp, 1, 4) || substr(timestamp, 6, 2) \
         ELSE 'unknown' \
       END AS yyyymm \
     FROM messages";

/// The negation of the same predicate, for the `messages_unknown` copy.
const UNKNOWN_COPY_WHERE: &str = "WHERE NOT (\
  length(timestamp) >= 7 \
  AND substr(timestamp, 5, 1) = '-' \
  AND substr(timestamp, 1, 4) GLOB '[0-9][0-9][0-9][0-9]' \
  AND substr(timestamp, 6, 2) GLOB '[0-9][0-9]'\
)";

/// Run the partitioning conversion.
///
/// Wrapped in a transaction by [`super::run_data_migration`] — which is why
/// every statement here goes through `execute`/`execute_batch` on single
/// statements and never through anything that would implicitly commit.
pub(super) fn apply(conn: &Connection, _hooks: &Hooks<'_>) -> rusqlite::Result<()> {
    // ── 1. Idempotency guard ─────────────────────────────────────────────
    let kind: Option<String> = conn
        .query_row(
            "SELECT type FROM sqlite_master WHERE name = 'messages'",
            [],
            |row| row.get(0),
        )
        .ok();
    if kind.as_deref() == Some("view") {
        return Ok(());
    }

    // ── 2. Discover months in existing data ──────────────────────────────
    let pre_count: i64 = conn.query_row("SELECT COUNT(*) FROM messages", [], |row| row.get(0))?;

    let mut months: Vec<String> = {
        let mut statement = conn.prepare(MONTH_DISCOVERY_SQL)?;
        let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
        let mut seen: Vec<String> = rows.collect::<rusqlite::Result<Vec<String>>>()?;
        seen.sort_unstable();
        seen.dedup();
        seen
    };

    if months.is_empty() {
        // Empty store — bootstrap with the current month so the view has at
        // least one source SELECT. DIV-302: this is a wall-clock read.
        months.push(current_month_utc());
    }

    // ── 3. Create partition tables ───────────────────────────────────────
    for month in &months {
        create_partition_table(conn, &format!("messages_{month}"))?;
    }

    // ── 4. Copy rows to partitions ───────────────────────────────────────
    let cols_csv = PARTITION_COLUMNS.join(", ");
    for month in &months {
        let partition = format!("messages_{month}");
        if month == "unknown" {
            conn.execute(
                &format!(
                    "INSERT OR IGNORE INTO {partition} ({cols_csv}) \
                     SELECT {cols_csv} FROM messages {UNKNOWN_COPY_WHERE}"
                ),
                [],
            )?;
        } else {
            conn.execute(
                &format!(
                    "INSERT OR IGNORE INTO {partition} ({cols_csv}) \
                     SELECT {cols_csv} FROM messages \
                     WHERE substr(timestamp, 1, 7) = ?"
                ),
                [dashed(month)],
            )?;
        }
    }

    // ── 5. Verify row counts ─────────────────────────────────────────────
    let mut post_count: i64 = 0;
    for month in &months {
        post_count += conn.query_row(
            &format!("SELECT COUNT(*) FROM messages_{month}"),
            [],
            |row| row.get::<_, i64>(0),
        )?;
    }
    if post_count != pre_count {
        return Err(migration_error(format!(
            "v008: partition copy lost rows — pre={pre_count} post={post_count}; rolling back"
        )));
    }

    let max_id: i64 = conn.query_row("SELECT COALESCE(MAX(id), 0) FROM messages", [], |row| {
        row.get(0)
    })?;

    // ── 6. Rebuild usage_events to drop the FK on messages(id) ───────────
    rebuild_usage_events_no_fk(conn)?;

    // ── 7. Drop the original messages table ──────────────────────────────
    conn.execute_batch("DROP TABLE messages")?;

    // ── 8. Create the messages view spanning every partition ─────────────
    rebuild_messages_view(conn)?;
    rebuild_messages_insert_trigger(conn)?;

    // ── 9. Create the global id sequence table ───────────────────────────
    // `concat!` and not a `\` continuation: Rust's line-continuation escape eats
    // the leading whitespace of the next line, and the reference's adjacent
    // string literals do NOT — the two spaces after `(` and after the comma are
    // in `sqlite_master`.
    conn.execute_batch(concat!(
        "CREATE TABLE _messages_id_seq (",
        "  rowid_kind INTEGER PRIMARY KEY CHECK (rowid_kind = 1),",
        "  next_id INTEGER NOT NULL",
        ")"
    ))?;
    conn.execute(
        "INSERT INTO _messages_id_seq (rowid_kind, next_id) VALUES (1, ?)",
        [max_id + 1],
    )?;
    Ok(())
}

/// `YYYYMM` → `YYYY-MM`, the form `substr(timestamp, 1, 7)` yields.
fn dashed(month: &str) -> String {
    format!("{}-{}", &month[..4], &month[4..])
}

/// `_create_partition_table` — one `messages_YYYYMM` plus its three indexes.
///
/// The literal below is Python's triple-quoted body with its indentation
/// intact; see the module doc for why that is load-bearing.
fn create_partition_table(conn: &Connection, partition: &str) -> rusqlite::Result<()> {
    if !valid_partition_name(partition) {
        return Err(migration_error(format!(
            "Invalid partition name: '{partition}'"
        )));
    }

    let existing: Option<String> = conn
        .query_row(
            "SELECT name FROM sqlite_master WHERE type = 'table' AND name = ?",
            [partition],
            |row| row.get(0),
        )
        .ok();
    if existing.is_some() {
        return Ok(());
    }

    conn.execute_batch(&format!(
        "CREATE TABLE {partition} (
            id                    INTEGER PRIMARY KEY,
            session_fk            INTEGER NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
            seq                   INTEGER NOT NULL,
            timestamp             TEXT NOT NULL,
            role                  TEXT NOT NULL,
            model                 TEXT,
            input_tokens          INTEGER NOT NULL DEFAULT 0,
            output_tokens         INTEGER NOT NULL DEFAULT 0,
            cache_create_tokens   INTEGER NOT NULL DEFAULT 0,
            cache_read_tokens     INTEGER NOT NULL DEFAULT 0,
            content_text          TEXT NOT NULL DEFAULT '',
            tools_json            TEXT NOT NULL DEFAULT '[]',
            raw_json              TEXT NOT NULL,
            is_sidechain          INTEGER NOT NULL DEFAULT 0,
            uuid                  TEXT,
            parent_uuid           TEXT,
            speed                 TEXT NOT NULL DEFAULT 'standard',
            UNIQUE (session_fk, seq)
        )"
    ))?;
    conn.execute_batch(&format!(
        "CREATE INDEX IF NOT EXISTS idx_{partition}_session_seq \
ON {partition}(session_fk, seq)"
    ))?;
    conn.execute_batch(&format!(
        "CREATE INDEX IF NOT EXISTS idx_{partition}_timestamp \
ON {partition}(timestamp)"
    ))?;
    conn.execute_batch(&format!(
        "CREATE INDEX IF NOT EXISTS idx_{partition}_model \
ON {partition}(model)"
    ))?;
    Ok(())
}

/// `_PARTITION_NAME_RE` — `^messages_(\d{6}|unknown)$`.
fn valid_partition_name(partition: &str) -> bool {
    let Some(tail) = partition.strip_prefix("messages_") else {
        return false;
    };
    tail == "unknown" || (tail.len() == 6 && tail.bytes().all(|byte| byte.is_ascii_digit()))
}

/// Every partition table, by name, sorted — `_rebuild_*`'s shared first step.
fn partition_names(conn: &Connection) -> rusqlite::Result<Vec<String>> {
    let mut statement = conn.prepare(
        "SELECT name FROM sqlite_master WHERE type = 'table' \
         AND (name GLOB 'messages_[0-9][0-9][0-9][0-9][0-9][0-9]' \
              OR name = 'messages_unknown')",
    )?;
    let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
    let mut names = rows.collect::<rusqlite::Result<Vec<String>>>()?;
    names.sort_unstable();
    Ok(names)
}

/// `_rebuild_messages_view` — drop and recreate the UNION ALL view.
fn rebuild_messages_view(conn: &Connection) -> rusqlite::Result<()> {
    let partitions = partition_names(conn)?;
    if partitions.is_empty() {
        return Ok(());
    }
    let cols_csv = PARTITION_COLUMNS.join(", ");
    let union_sql = partitions
        .iter()
        .map(|partition| format!("SELECT {cols_csv} FROM {partition}"))
        .collect::<Vec<_>>()
        .join(" UNION ALL ");
    conn.execute_batch("DROP VIEW IF EXISTS messages")?;
    conn.execute_batch(&format!("CREATE VIEW messages AS {union_sql}"))?;
    Ok(())
}

/// `_rebuild_messages_insert_trigger` — the INSTEAD OF INSERT router.
///
/// The whole body is one generated string and it lands in `sqlite_master`, so
/// the concatenation order below is the reference's exactly: the sequence bump
/// first, then one `INSERT OR IGNORE` per known month in sorted order, then the
/// `messages_unknown` fallback.
fn rebuild_messages_insert_trigger(conn: &Connection) -> rusqlite::Result<()> {
    let mut partitions = partition_names(conn)?;
    // The fallback target must exist even when the store's data never produced
    // it — the fully-populated bootstrap path skips it otherwise.
    if !partitions.iter().any(|name| name == "messages_unknown") {
        create_partition_table(conn, "messages_unknown")?;
        partitions.push("messages_unknown".to_owned());
        partitions.sort_unstable();
        partitions.dedup();
        rebuild_messages_view(conn)?;
    }

    let cols_csv = PARTITION_COLUMNS.join(", ");
    let base_select = PARTITION_COLUMNS
        .iter()
        .map(|column| {
            if *column == "id" {
                "COALESCE(NEW.id, (SELECT next_id - 1 FROM _messages_id_seq WHERE rowid_kind = 1))"
                    .to_owned()
            } else if let Some((_, literal)) =
                COLUMN_DEFAULTS.iter().find(|(name, _)| name == column)
            {
                format!("COALESCE(NEW.{column}, {literal})")
            } else {
                format!("NEW.{column}")
            }
        })
        .collect::<Vec<_>>()
        .join(", ");

    let known_months: Vec<&str> = partitions
        .iter()
        .filter(|name| *name != "messages_unknown")
        .map(|name| &name["messages_".len()..])
        .collect();

    let mut inserts = String::new();
    for month in &known_months {
        inserts.push_str(&format!(
            "INSERT OR IGNORE INTO messages_{month} ({cols_csv}) \
             SELECT {base_select} \
             WHERE substr(NEW.timestamp, 1, 7) = '{dashed}';",
            dashed = dashed(month)
        ));
    }
    let fallback_where = if known_months.is_empty() {
        "1 = 1".to_owned()
    } else {
        let known_list = known_months
            .iter()
            .map(|month| format!("'{}'", dashed(month)))
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "length(NEW.timestamp) < 7 \
             OR substr(NEW.timestamp, 5, 1) <> '-' \
             OR substr(NEW.timestamp, 1, 7) NOT IN ({known_list})"
        )
    };
    inserts.push_str(&format!(
        "INSERT OR IGNORE INTO messages_unknown ({cols_csv}) \
         SELECT {base_select} \
         WHERE {fallback_where};"
    ));

    // `concat!` for the same reason `_messages_id_seq` uses it: the two-space
    // runs are the reference's adjacent literals, and they land in the trigger
    // body that `.schema` prints.
    let bump_sql = concat!(
        "UPDATE _messages_id_seq SET next_id = MAX(",
        "  next_id + (CASE WHEN NEW.id IS NULL THEN 1 ELSE 0 END),",
        "  COALESCE(NEW.id + 1, next_id)",
        ") WHERE rowid_kind = 1;"
    );

    conn.execute_batch("DROP TRIGGER IF EXISTS messages_insert_route")?;
    conn.execute_batch(&format!(
        "CREATE TRIGGER messages_insert_route INSTEAD OF INSERT ON messages \
         BEGIN {bump_sql}{inserts} END"
    ))?;
    Ok(())
}

/// `_rebuild_usage_events_no_fk` — the four-step FK-drop dance plus indexes.
fn rebuild_usage_events_no_fk(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE usage_events_new (
            id                  INTEGER PRIMARY KEY,
            source_message_fk   INTEGER NOT NULL,
            provider            TEXT    NOT NULL,
            account             TEXT    NOT NULL DEFAULT 'default',
            project_id          INTEGER NOT NULL REFERENCES projects(id),
            session_id          TEXT    NOT NULL,
            ts                  TEXT    NOT NULL,
            day                 TEXT    NOT NULL,
            model               TEXT    NOT NULL DEFAULT '',
            speed               TEXT    NOT NULL DEFAULT 'standard',
            input_tokens        INTEGER NOT NULL DEFAULT 0,
            output_tokens       INTEGER NOT NULL DEFAULT 0,
            cache_read_tokens   INTEGER NOT NULL DEFAULT 0,
            cache_create_tokens INTEGER NOT NULL DEFAULT 0,
            cost_usd            REAL    NOT NULL DEFAULT 0.0,
            cost_source         TEXT    NOT NULL DEFAULT 'rate_card',
            role                TEXT    NOT NULL,
            raw_extras          TEXT
        )",
    )?;
    conn.execute_batch(
        "INSERT INTO usage_events_new (
            id, source_message_fk, provider, account, project_id,
            session_id, ts, day, model, speed,
            input_tokens, output_tokens, cache_read_tokens, cache_create_tokens,
            cost_usd, cost_source, role, raw_extras
        )
        SELECT
            id, source_message_fk, provider, account, project_id,
            session_id, ts, day, model, speed,
            input_tokens, output_tokens, cache_read_tokens, cache_create_tokens,
            cost_usd, cost_source, role, raw_extras
        FROM usage_events",
    )?;
    conn.execute_batch("DROP TABLE usage_events")?;
    conn.execute_batch("ALTER TABLE usage_events_new RENAME TO usage_events")?;
    conn.execute_batch("CREATE INDEX idx_events_day      ON usage_events(day)")?;
    conn.execute_batch("CREATE INDEX idx_events_project  ON usage_events(project_id, day)")?;
    conn.execute_batch("CREATE INDEX idx_events_provider ON usage_events(provider, day)")?;
    conn.execute_batch("CREATE INDEX idx_events_session  ON usage_events(session_id)")?;
    conn.execute_batch("CREATE INDEX idx_events_model    ON usage_events(model, day)")?;
    conn.execute_batch("CREATE UNIQUE INDEX uniq_events_msg ON usage_events(source_message_fk)")?;
    Ok(())
}

/// `datetime.now(UTC).strftime("%Y%m")` — DIV-302's clock.
///
/// Civil-from-days, Howard Hinnant's algorithm, because the workspace has no
/// date crate and this is the only date arithmetic `stax-core` needs.
fn current_month_utc() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |delta| delta.as_secs());
    let days = i64::try_from(secs / 86_400).unwrap_or(0);
    let (year, month) = civil_from_days(days);
    format!("{year:04}{month:02}")
}

/// Days since 1970-01-01 → `(year, month)`.
fn civil_from_days(days: i64) -> (i64, u32) {
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let doe = shifted.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { year + 1 } else { year };
    (year, u32::try_from(month).unwrap_or(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store_at_v7() -> Connection {
        let conn = Connection::open_in_memory().expect("in-memory store");
        crate::schema::tests::apply_to(&conn, 7);
        conn
    }

    fn seed_message(conn: &Connection, session_fk: i64, seq: i64, timestamp: &str) {
        conn.execute(
            "INSERT INTO messages (session_fk, seq, timestamp, role, raw_json) \
             VALUES (?, ?, ?, 'user', '{}')",
            rusqlite::params![session_fk, seq, timestamp],
        )
        .expect("message");
    }

    fn seed_session(conn: &Connection) -> i64 {
        conn.execute(
            "INSERT INTO projects (provider, slug, path, display_name, first_seen, last_modified) \
             VALUES ('claude', 'p', NULL, 'p', 0.0, 0.0)",
            [],
        )
        .expect("project");
        let project_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO sessions (project_id, session_id) VALUES (?, 's')",
            [project_id],
        )
        .expect("session");
        conn.last_insert_rowid()
    }

    fn object_sql(conn: &Connection, name: &str) -> String {
        conn.query_row(
            "SELECT COALESCE(sql, '') FROM sqlite_master WHERE name = ?",
            [name],
            |row| row.get(0),
        )
        .unwrap_or_default()
    }

    #[test]
    fn the_partition_name_rule_matches_the_reference_regex() {
        assert!(valid_partition_name("messages_202601"));
        assert!(valid_partition_name("messages_unknown"));
        assert!(!valid_partition_name("messages_2026"));
        assert!(!valid_partition_name("messages_20260a"));
        assert!(!valid_partition_name("messages_"));
        assert!(!valid_partition_name("sessions_202601"));
        assert!(!valid_partition_name("xmessages_202601"));
    }

    #[test]
    fn rows_land_in_their_month_and_garbage_lands_in_unknown() {
        let conn = store_at_v7();
        let session = seed_session(&conn);
        seed_message(&conn, session, 0, "2026-01-15T00:00:00Z");
        seed_message(&conn, session, 1, "2026-01-16T00:00:00Z");
        seed_message(&conn, session, 2, "2026-02-01T00:00:00Z");
        seed_message(&conn, session, 3, "");
        seed_message(&conn, session, 4, "not-a-timestamp");

        apply(&conn, &Hooks::default()).expect("v008");

        let count = |table: &str| -> i64 {
            conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .expect("count")
        };
        assert_eq!(count("messages_202601"), 2);
        assert_eq!(count("messages_202602"), 1);
        assert_eq!(count("messages_unknown"), 2);
        assert_eq!(count("messages"), 5, "the view spans every partition");
    }

    #[test]
    fn the_sequence_continues_the_existing_ids() {
        let conn = store_at_v7();
        let session = seed_session(&conn);
        seed_message(&conn, session, 0, "2026-01-15T00:00:00Z");
        seed_message(&conn, session, 1, "2026-01-16T00:00:00Z");
        let max_before: i64 = conn
            .query_row("SELECT MAX(id) FROM messages", [], |row| row.get(0))
            .expect("max");

        apply(&conn, &Hooks::default()).expect("v008");

        let next: i64 = conn
            .query_row("SELECT next_id FROM _messages_id_seq", [], |row| row.get(0))
            .expect("next_id");
        assert_eq!(next, max_before + 1);
    }

    #[test]
    fn the_trigger_routes_a_raw_insert_into_the_view() {
        let conn = store_at_v7();
        let session = seed_session(&conn);
        seed_message(&conn, session, 0, "2026-01-15T00:00:00Z");
        apply(&conn, &Hooks::default()).expect("v008");

        // Exactly what the fixtures across the codebase do after v008.
        conn.execute(
            "INSERT INTO messages (session_fk, seq, timestamp, role, raw_json) \
             VALUES (?, 9, '2026-01-20T00:00:00Z', 'user', '{}')",
            [session],
        )
        .expect("insert through the view");
        let in_january: i64 = conn
            .query_row("SELECT COUNT(*) FROM messages_202601", [], |row| row.get(0))
            .expect("count");
        assert_eq!(in_january, 2);

        conn.execute(
            "INSERT INTO messages (session_fk, seq, timestamp, role, raw_json) \
             VALUES (?, 10, 'garbage', 'user', '{}')",
            [session],
        )
        .expect("insert through the view");
        let unknown: i64 = conn
            .query_row("SELECT COUNT(*) FROM messages_unknown", [], |row| {
                row.get(0)
            })
            .expect("count");
        assert_eq!(unknown, 1, "an unroutable timestamp is never dropped");
    }

    #[test]
    fn the_trigger_applies_the_not_null_defaults() {
        let conn = store_at_v7();
        let session = seed_session(&conn);
        seed_message(&conn, session, 0, "2026-01-15T00:00:00Z");
        apply(&conn, &Hooks::default()).expect("v008");

        conn.execute(
            "INSERT INTO messages (session_fk, seq, timestamp, role, raw_json) \
             VALUES (?, 9, '2026-01-20T00:00:00Z', 'user', '{}')",
            [session],
        )
        .expect("insert");
        let (tools, speed, tokens): (String, String, i64) = conn
            .query_row(
                "SELECT tools_json, speed, input_tokens FROM messages WHERE seq = 9",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("row");
        assert_eq!(tools, "[]");
        assert_eq!(speed, "standard");
        assert_eq!(tokens, 0);
    }

    #[test]
    fn an_already_partitioned_store_is_left_alone() {
        let conn = store_at_v7();
        let session = seed_session(&conn);
        seed_message(&conn, session, 0, "2026-01-15T00:00:00Z");
        apply(&conn, &Hooks::default()).expect("first");
        let before = object_sql(&conn, "messages_insert_route");
        apply(&conn, &Hooks::default()).expect("second is the idempotency guard");
        assert_eq!(before, object_sql(&conn, "messages_insert_route"));
    }

    #[test]
    fn usage_events_keeps_its_dedup_index_and_loses_the_messages_fk() {
        let conn = store_at_v7();
        apply(&conn, &Hooks::default()).expect("v008");
        let sql = object_sql(&conn, "usage_events");
        assert!(
            !sql.contains("REFERENCES messages"),
            "the FK on a view is unenforceable and must be gone: {sql}"
        );
        assert!(
            sql.contains("REFERENCES projects(id)"),
            "the projects FK stays: {sql}"
        );
        assert_eq!(
            object_sql(&conn, "uniq_events_msg"),
            "CREATE UNIQUE INDEX uniq_events_msg ON usage_events(source_message_fk)"
        );
    }

    #[test]
    fn the_index_names_keep_their_column_alignment() {
        // Runs of spaces inside a `CREATE INDEX` are stored verbatim; three
        // different run lengths appear in the reference and all three are
        // compared by the schema differ.
        let conn = store_at_v7();
        apply(&conn, &Hooks::default()).expect("v008");
        assert_eq!(
            object_sql(&conn, "idx_events_day"),
            "CREATE INDEX idx_events_day      ON usage_events(day)"
        );
        assert_eq!(
            object_sql(&conn, "idx_events_provider"),
            "CREATE INDEX idx_events_provider ON usage_events(provider, day)"
        );
        assert_eq!(
            object_sql(&conn, "idx_events_model"),
            "CREATE INDEX idx_events_model    ON usage_events(model, day)"
        );
    }

    #[test]
    fn an_empty_store_bootstraps_one_month_plus_unknown() {
        let conn = store_at_v7();
        apply(&conn, &Hooks::default()).expect("v008");
        let partitions = partition_names(&conn).expect("partitions");
        assert_eq!(partitions.len(), 2, "{partitions:?}");
        assert_eq!(partitions[1], "messages_unknown");
        assert_eq!(
            partitions[0],
            format!("messages_{}", current_month_utc()),
            "DIV-302: the bootstrap partition is named from the wall clock"
        );
    }

    #[test]
    fn civil_from_days_agrees_with_known_dates() {
        assert_eq!(civil_from_days(0), (1970, 1));
        assert_eq!(civil_from_days(31), (1970, 2));
        assert_eq!(civil_from_days(59), (1970, 3));
        // 2026-08-01 is 20_666 days after the epoch.
        assert_eq!(civil_from_days(20_666), (2026, 8));
        // 2024-02-29 — the leap-day leg.
        assert_eq!(civil_from_days(19_782), (2024, 2));
    }
}
