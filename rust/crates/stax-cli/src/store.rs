//! `stax store` — wave 0's runnable proof, and the observe wire.
//!
//! Bare `stax store` opens the store read-only, reads `PRAGMA user_version`,
//! and prints one row per `sqlite_master` table or view with its `COUNT(*)`,
//! sorted by name. The output shape is fixed on purpose: the wave-0 gate is
//! that it matches, byte for byte, a Python reader doing the same thing
//! against the same file.
//!
//! `stax store tail` is agent-remotes Phase 2's remote half: new `messages`
//! rows for one session (the most recent by `last_ts` when none is named),
//! strictly after `--since-seq`, as text or as a versioned
//! `stackunderflow.observe/1` envelope. `stax observe <remote>` runs exactly
//! this verb over ssh and renders the batches — which is why it exists here,
//! on the store, rather than inside observe: both ends of the wire ship the
//! same binary, and a remote that predates the verb fails loudly with clap's
//! unknown-subcommand error, the version-skew signal observe reports.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{Args, Subcommand};
use rusqlite::Connection;
use stax_core::settings;
use stax_core::store::{ObjectCount, Store};

/// Column widths for the printed table. The Python reference uses the same
/// three, so a diff of the two outputs is empty rather than merely equivalent.
const NAME_WIDTH: usize = 40;
const KIND_WIDTH: usize = 6;
const ROWS_WIDTH: usize = 12;

/// Arguments for `stax store`.
#[derive(Debug, Args)]
pub struct StoreArgs {
    /// Store to read. Defaults to `$STACKUNDERFLOW_HOME/store.db`, else
    /// `~/.stackunderflow/store.db`.
    #[arg(long, value_name = "PATH", global = true)]
    pub store: Option<PathBuf>,

    /// Optional subverb; bare `store` keeps printing the wave-0 table.
    #[command(subcommand)]
    pub verb: Option<StoreVerb>,
}

/// The `store` subverbs.
#[derive(Debug, Subcommand)]
pub enum StoreVerb {
    /// New messages for one session, after a sequence number — the observe
    /// wire. Read-only.
    Tail(TailArgs),
}

/// `store tail`'s flags.
#[derive(Debug, Args)]
pub struct TailArgs {
    /// Session id to tail. Default: the most recent session in the store.
    #[arg(long, value_name = "SESSION_ID")]
    pub session: Option<String>,
    /// Only rows with seq strictly greater than this.
    #[arg(long = "since-seq", value_name = "N", default_value_t = 0)]
    pub since_seq: i64,
    /// Max rows per call.
    #[arg(long, value_name = "N", default_value_t = 50)]
    pub limit: i64,
    /// Emit the stackunderflow.observe/1 envelope instead of text.
    #[arg(long, action = clap::ArgAction::SetTrue)]
    pub json: bool,
}

/// Run `store`, printing the table (or a tail batch) to stdout.
///
/// # Errors
/// When the store is missing or SQLite refuses to read it.
pub fn run_store(args: &StoreArgs) -> Result<()> {
    let path = match &args.store {
        Some(path) => path.clone(),
        None => settings::store_path(),
    };
    let store = Store::open_read_only(&path)?;
    match &args.verb {
        None => {
            let version = store.schema_version()?;
            let objects = store.object_counts()?;
            print!("{}", render_store(store.path(), version, &objects));
        }
        Some(StoreVerb::Tail(tail)) => {
            print!("{}", run_tail(store.conn(), tail)?);
        }
    }
    Ok(())
}

// ── store tail ───────────────────────────────────────────────────────────────

/// One tailed message row.
#[derive(Debug, PartialEq)]
pub struct TailRow {
    pub seq: i64,
    pub role: String,
    pub ts: String,
    pub text: String,
}

/// The tail body: resolve the session, fetch the batch, render.
///
/// # Errors
/// When no session exists at all, or a query fails.
pub fn run_tail(conn: &Connection, args: &TailArgs) -> Result<String> {
    let session_id = match &args.session {
        Some(id) => id.clone(),
        None => latest_session(conn)?
            .context("the store has no sessions to tail")?,
    };
    let rows = tail_rows(conn, &session_id, args.since_seq, args.limit)?;
    Ok(if args.json {
        render_tail_json(&session_id, args.since_seq, &rows)
    } else {
        render_tail_text(&session_id, &rows)
    })
}

/// The most recent session in the store, by `last_ts`.
///
/// # Errors
/// When the query fails (an empty store answers `Ok(None)`).
pub fn latest_session(conn: &Connection) -> Result<Option<String>> {
    conn.query_row(
        "SELECT session_id FROM sessions ORDER BY last_ts DESC, id DESC LIMIT 1",
        [],
        |row| row.get(0),
    )
    .map(Some)
    .or_else(|error| match error {
        rusqlite::Error::QueryReturnedNoRows => Ok(None),
        other => Err(other.into()),
    })
}

/// Rows for `session_id` strictly after `since_seq`, oldest first.
///
/// # Errors
/// When the query fails.
pub fn tail_rows(
    conn: &Connection,
    session_id: &str,
    since_seq: i64,
    limit: i64,
) -> Result<Vec<TailRow>> {
    let mut statement = conn.prepare(
        "SELECT m.seq, COALESCE(m.role, ''), COALESCE(m.timestamp, ''), \
                COALESCE(m.content_text, '') \
         FROM messages m JOIN sessions s ON s.id = m.session_fk \
         WHERE s.session_id = ?1 AND m.seq > ?2 \
         ORDER BY m.seq LIMIT ?3",
    )?;
    let rows = statement
        .query_map(rusqlite::params![session_id, since_seq, limit], |row| {
            Ok(TailRow {
                seq: row.get(0)?,
                role: row.get(1)?,
                ts: row.get(2)?,
                text: row.get(3)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// The text rendering: one line per row, a header naming the session.
#[must_use]
pub fn render_tail_text(session_id: &str, rows: &[TailRow]) -> String {
    let mut out = format!("session: {session_id}\n");
    for row in rows {
        let mut text = row.text.replace('\n', " ");
        if text.chars().count() > 200 {
            text = text.chars().take(200).collect::<String>() + "…";
        }
        let _ = writeln!(out, "[{} {} #{}] {}", row.ts, row.role, row.seq, text);
    }
    out
}

/// The `stackunderflow.observe/1` envelope.
#[must_use]
pub fn render_tail_json(session_id: &str, since_seq: i64, rows: &[TailRow]) -> String {
    let mut body = serde_json::Map::new();
    body.insert(
        "schema".to_owned(),
        serde_json::Value::from("stackunderflow.observe/1"),
    );
    body.insert("session_id".to_owned(), serde_json::Value::from(session_id));
    body.insert("since_seq".to_owned(), serde_json::Value::from(since_seq));
    body.insert(
        "last_seq".to_owned(),
        serde_json::Value::from(rows.last().map_or(since_seq, |row| row.seq)),
    );
    body.insert(
        "row_count".to_owned(),
        serde_json::Value::from(rows.len() as i64),
    );
    body.insert(
        "rows".to_owned(),
        serde_json::Value::Array(
            rows.iter()
                .map(|row| {
                    let mut entry = serde_json::Map::new();
                    entry.insert("seq".to_owned(), serde_json::Value::from(row.seq));
                    entry.insert("role".to_owned(), serde_json::Value::from(row.role.as_str()));
                    entry.insert("ts".to_owned(), serde_json::Value::from(row.ts.as_str()));
                    entry.insert("text".to_owned(), serde_json::Value::from(row.text.as_str()));
                    serde_json::Value::Object(entry)
                })
                .collect(),
        ),
    );
    format!(
        "{}\n",
        stax_memory::pyjson::dumps_pretty(&serde_json::Value::Object(body))
    )
}

/// Render the status table.
///
/// Kept separate from the I/O so the exact bytes can be asserted in a test that
/// needs no database at all.
#[must_use]
pub fn render_store(path: &Path, schema_version: i64, objects: &[ObjectCount]) -> String {
    let mut out = String::with_capacity(96 + objects.len() * (NAME_WIDTH + 24));
    let _ = writeln!(out, "store: {}", path.display());
    let _ = writeln!(out, "schema: v{schema_version:03}");
    let _ = writeln!(out, "objects: {}", objects.len());
    out.push('\n');
    let _ = writeln!(
        out,
        "{:<NAME_WIDTH$} {:<KIND_WIDTH$} {:>ROWS_WIDTH$}",
        "NAME", "KIND", "ROWS"
    );
    for object in objects {
        let _ = writeln!(
            out,
            "{:<NAME_WIDTH$} {:<KIND_WIDTH$} {:>ROWS_WIDTH$}",
            object.name,
            object.kind.as_str(),
            object.rows
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use stax_core::store::ObjectKind;

    use super::*;

    fn sample() -> Vec<ObjectCount> {
        vec![
            ObjectCount {
                name: "messages".into(),
                kind: ObjectKind::View,
                rows: 383_263,
            },
            ObjectCount {
                name: "messages_202601".into(),
                kind: ObjectKind::Table,
                rows: 66_236,
            },
            ObjectCount {
                name: "sessions".into(),
                kind: ObjectKind::Table,
                rows: 3_566,
            },
        ]
    }

    #[test]
    fn renders_the_exact_bytes_python_prints() {
        let rendered = render_store(Path::new("/data/su/store.db"), 30, &sample());
        let expected = concat!(
            "store: /data/su/store.db\n",
            "schema: v030\n",
            "objects: 3\n",
            "\n",
            "NAME                                     KIND           ROWS\n",
            "messages                                 view         383263\n",
            "messages_202601                          table         66236\n",
            "sessions                                 table          3566\n",
        );
        assert_eq!(rendered, expected);
    }

    #[test]
    fn the_view_is_tagged_as_a_view() {
        let rendered = render_store(Path::new("/x.db"), 30, &sample());
        let messages = rendered
            .lines()
            .find(|line| line.starts_with("messages "))
            .expect("the messages row");
        assert!(messages.contains("view"), "{messages}");
    }

    #[test]
    fn an_empty_store_still_renders_a_header() {
        let rendered = render_store(Path::new("/x.db"), 0, &[]);
        assert_eq!(
            rendered,
            concat!(
                "store: /x.db\n",
                "schema: v000\n",
                "objects: 0\n",
                "\n",
                "NAME                                     KIND           ROWS\n",
            )
        );
    }

    fn tail_fixture() -> Connection {
        // The two surfaces `tail` touches, shaped like the store's: `messages`
        // is a view over monthly tables in the real schema, but the query only
        // needs the columns, so a plain table stands in.
        let conn = Connection::open_in_memory().expect("in-memory db");
        conn.execute_batch(
            "CREATE TABLE sessions(id INTEGER PRIMARY KEY, session_id TEXT, last_ts TEXT);
             CREATE TABLE messages(session_fk INTEGER, seq INTEGER, timestamp TEXT,
                                   role TEXT, content_text TEXT);
             INSERT INTO sessions VALUES (1, 'old-session', '2026-08-01T00:00:00+00:00');
             INSERT INTO sessions VALUES (2, 'live-session', '2026-08-10T12:00:00+00:00');
             INSERT INTO messages VALUES (2, 1, '2026-08-10T12:00:01+00:00', 'user', 'first');
             INSERT INTO messages VALUES (2, 2, '2026-08-10T12:00:02+00:00', 'assistant', 'second');
             INSERT INTO messages VALUES (1, 9, '2026-08-01T00:00:09+00:00', 'user', 'other session');",
        )
        .expect("fixture");
        conn
    }

    #[test]
    fn tail_picks_the_most_recent_session_and_advances_by_seq() {
        let conn = tail_fixture();
        assert_eq!(
            latest_session(&conn).expect("query"),
            Some("live-session".to_owned())
        );
        let rows = tail_rows(&conn, "live-session", 0, 50).expect("rows");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].text, "first");
        // The cursor contract: strictly-greater-than, so re-polling with
        // last_seq returns only what is new.
        let rows = tail_rows(&conn, "live-session", 1, 50).expect("rows");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].seq, 2);
        let rows = tail_rows(&conn, "live-session", 2, 50).expect("rows");
        assert!(rows.is_empty());
    }

    #[test]
    fn the_observe_envelope_carries_the_cursor() {
        let conn = tail_fixture();
        let rows = tail_rows(&conn, "live-session", 0, 50).expect("rows");
        let body = render_tail_json("live-session", 0, &rows);
        assert!(body.contains("\"schema\": \"stackunderflow.observe/1\""), "{body}");
        assert!(body.contains("\"last_seq\": 2"), "{body}");
        // An empty batch keeps the caller's cursor instead of resetting it.
        let body = render_tail_json("live-session", 7, &[]);
        assert!(body.contains("\"last_seq\": 7"), "{body}");
        assert!(body.contains("\"row_count\": 0"), "{body}");
    }

    #[test]
    fn an_empty_store_tails_to_a_named_error() {
        let conn = Connection::open_in_memory().expect("db");
        conn.execute_batch(
            "CREATE TABLE sessions(id INTEGER PRIMARY KEY, session_id TEXT, last_ts TEXT);",
        )
        .expect("schema");
        assert_eq!(latest_session(&conn).expect("query"), None);
    }

    #[test]
    fn long_names_push_the_columns_instead_of_truncating() {
        // Python's f-string padding does not truncate either; matching the
        // overflow behavior is what keeps the byte-diff empty on any store.
        let name = "a".repeat(NAME_WIDTH + 5);
        let rendered = render_store(
            Path::new("/x.db"),
            30,
            &[ObjectCount {
                name: name.clone(),
                kind: ObjectKind::Table,
                rows: 1,
            }],
        );
        let row = rendered.lines().last().expect("the row");
        // 13 = the kind column's trailing pad (1) + the separator (1) + the
        // right-aligned rows column's lead (11).
        assert_eq!(row, format!("{name} table{}1", " ".repeat(13)));
    }
}
