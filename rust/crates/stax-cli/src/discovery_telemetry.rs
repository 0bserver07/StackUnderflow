//! `stax discovery telemetry | demote-uncited` — `cli.py:4482`–`:4620`, over
//! `services/discovery_telemetry.py`'s three introspection functions.
//!
//! | Item | Python | Used by |
//! |---|---|---|
//! | [`iter_telemetry`] | `discovery_telemetry.iter_telemetry` | `discovery telemetry` |
//! | [`demote_candidates`] | `discovery_telemetry.demote_candidates` | `discovery demote-uncited` |
//! | [`mark_demoted`] | `discovery_telemetry.mark_demoted` | `discovery demote-uncited` (non-dry-run) |
//!
//! The other seven functions in that module (`record_loaded`, `record_cited`,
//! `cite_rate`, `cite_rate_terms`, `telemetry_enabled`, …) are the *write* and
//! *ranking* halves and have **no CLI caller**; they stay unported for the
//! reason DIV-009 B-2 already records — this binary opens the store read-only
//! on the discovery read path, so the counter bump cannot happen and no ranking
//! term reads the result. These three are here rather than in a shared crate
//! for the mirror-image reason: the CLI is their only consumer in either
//! implementation, and a shared home for a single-consumer helper is the
//! premature half of the "one owner" rule.
//!
//! # Every read is `except sqlite3.Error: return []`
//!
//! All three functions swallow SQLite errors. That is not defensive noise: on a
//! store predating migration v009 the table does not exist, and the reference
//! answers "no rows" rather than a traceback. Reproduced as `Result::ok()` at
//! the same three sites, and it is why these functions are infallible here.
//!
//! # `demote_candidates` reads a WALL CLOCK inside SQL
//!
//! `julianday('now') - julianday(first_loaded_ts) >= ?` is evaluated by SQLite
//! at query time, so the two implementations evaluate it milliseconds apart. A
//! row whose age sits exactly on the `--min-age-days` boundary can therefore
//! answer differently on the two sides. Recorded (DIV-411) rather than
//! engineered around: pinning it would mean passing a fixed `now` the reference
//! has no parameter for, i.e. changing behaviour to make a differ happy.
//!
//! # `mark_demoted` commits, because `db.connect` is autocommit
//!
//! `sqlite3.connect(path, isolation_level=None)` — so the `executemany` lands
//! on disk before `conn.close()` and there is no explicit `commit()` to port.
//! Had `isolation_level` been the default, closing without committing would
//! have rolled the whole demotion back and the verb would print a count it
//! never persisted. Checked in `store/db.py` rather than assumed.

use anyhow::Result;
use clap::{Args, Subcommand};
use rusqlite::Connection;
use serde_json::{Map, Value};

use crate::click::Output;
use crate::memory::py_int;
use crate::reports::open_store;
use stax_core::queries::pyint::PyInt;

/// One `discovery_telemetry` row, in the table's column order.
///
/// The order is load-bearing: `iter_telemetry` builds `dict(row)` from
/// `SELECT *`, `json.dumps` is called **without** `sort_keys`, and the CLI's
/// JSON leg therefore ships the columns in DDL order with `cite_rate` appended
/// last (it is a new key) and `demoted` in place (it is an overwrite).
#[derive(Debug, Clone)]
pub struct TelemetryRow {
    /// `find_sessions_in_path` | `find_sessions_touching_file` | `search_past_decisions`.
    pub command: String,
    /// The surfaced session's id.
    pub session_id: String,
    /// Times this pair was surfaced.
    pub loaded_count: i64,
    /// Times it was then looked up.
    pub cited_count: i64,
    /// ISO-8601 UTC, or `None`.
    pub first_loaded_ts: Option<String>,
    /// ISO-8601 UTC, or `None`.
    pub last_loaded_ts: Option<String>,
    /// ISO-8601 UTC, or `None`.
    pub last_cited_ts: Option<String>,
    /// `bool(demoted)` — an integer column read as Python truthiness.
    pub demoted: bool,
    /// `cited / loaded` when `loaded > 0`, else `0.0`.
    pub cite_rate: f64,
}

impl TelemetryRow {
    /// The dict `iter_telemetry` yields, in the order `json.dumps` writes it.
    #[must_use]
    pub fn to_value(&self) -> Value {
        let text = |value: &Option<String>| {
            value
                .as_ref()
                .map_or(Value::Null, |raw| Value::from(raw.clone()))
        };
        let mut out = Map::new();
        out.insert("command".to_owned(), Value::from(self.command.clone()));
        out.insert(
            "session_id".to_owned(),
            Value::from(self.session_id.clone()),
        );
        out.insert("loaded_count".to_owned(), Value::from(self.loaded_count));
        out.insert("cited_count".to_owned(), Value::from(self.cited_count));
        out.insert("first_loaded_ts".to_owned(), text(&self.first_loaded_ts));
        out.insert("last_loaded_ts".to_owned(), text(&self.last_loaded_ts));
        out.insert("last_cited_ts".to_owned(), text(&self.last_cited_ts));
        out.insert("demoted".to_owned(), Value::Bool(self.demoted));
        out.insert("cite_rate".to_owned(), Value::from(self.cite_rate));
        Value::Object(out)
    }
}

/// `iter_telemetry(conn, command=…, session_id=…, limit=…)`.
///
/// `if command:` / `if session_id:` are Python truthiness, so an EMPTY string
/// filters nothing — the `--project ''` class, twice-proven in tranche 1 and
/// rowed below. `limit <= 0` means no limit, and so does `limit == 0`, because
/// the guard is `if limit and limit > 0`.
#[must_use]
pub fn iter_telemetry(
    conn: &Connection,
    command: Option<&str>,
    session_id: Option<&str>,
    limit: i64,
) -> Vec<TelemetryRow> {
    let mut sql = "SELECT * FROM discovery_telemetry".to_owned();
    // The parameter list is heterogeneous (two TEXTs then an INTEGER), which is
    // what `params: list[object]` is on the reference; `Box<dyn ToSql>` is the
    // same list with the types kept.
    let mut params: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
    let mut where_clauses: Vec<&str> = Vec::new();
    if let Some(value) = command.filter(|value| !value.is_empty()) {
        where_clauses.push("command = ?");
        params.push(Box::new(value.to_owned()));
    }
    if let Some(value) = session_id.filter(|value| !value.is_empty()) {
        where_clauses.push("session_id = ?");
        params.push(Box::new(value.to_owned()));
    }
    if !where_clauses.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&where_clauses.join(" AND "));
    }
    // Nulls last, then newest first, then the two key columns ascending.
    sql.push_str(" ORDER BY last_loaded_ts IS NULL, last_loaded_ts DESC, command, session_id");
    if limit > 0 {
        sql.push_str(" LIMIT ?");
        params.push(Box::new(limit));
    }

    let bound: Vec<&dyn rusqlite::ToSql> = params.iter().map(std::convert::AsRef::as_ref).collect();
    let Ok(mut stmt) = conn.prepare(&sql) else {
        return Vec::new();
    };
    let Ok(rows) = stmt.query_map(bound.as_slice(), |row| {
        // `int(d.get("loaded_count") or 0)` — a NULL count is 0, not an error.
        let loaded: i64 = row.get::<_, Option<i64>>("loaded_count")?.unwrap_or(0);
        let cited: i64 = row.get::<_, Option<i64>>("cited_count")?.unwrap_or(0);
        Ok(TelemetryRow {
            command: row.get::<_, Option<String>>("command")?.unwrap_or_default(),
            session_id: row
                .get::<_, Option<String>>("session_id")?
                .unwrap_or_default(),
            loaded_count: loaded,
            cited_count: cited,
            first_loaded_ts: row.get("first_loaded_ts")?,
            last_loaded_ts: row.get("last_loaded_ts")?,
            last_cited_ts: row.get("last_cited_ts")?,
            // `bool(d.get("demoted"))` — Python truthiness over the integer.
            demoted: row.get::<_, Option<i64>>("demoted")?.unwrap_or(0) != 0,
            #[allow(clippy::cast_precision_loss)]
            cite_rate: if loaded > 0 {
                cited as f64 / loaded as f64
            } else {
                0.0
            },
        })
    }) else {
        return Vec::new();
    };
    rows.filter_map(std::result::Result::ok).collect()
}

/// `demote_candidates(conn, min_loads=…, min_age_days=…)`.
///
/// `(command, session_id, loaded_count)`, worst offenders first. The four
/// predicates are `loaded_count >= min_loads`, `cited_count = 0`, `demoted =
/// 0`, and an age of at least `min_age_days` — measured by SQLite's own
/// `julianday('now')`, i.e. a clock (see the module docs).
#[must_use]
pub fn demote_candidates(
    conn: &Connection,
    min_loads: i64,
    min_age_days: i64,
) -> Vec<(String, String, i64)> {
    let Ok(mut stmt) = conn.prepare(
        "SELECT command, session_id, loaded_count FROM discovery_telemetry \
         WHERE loaded_count >= ? \
           AND cited_count = 0 \
           AND demoted = 0 \
           AND first_loaded_ts IS NOT NULL \
           AND julianday('now') - julianday(first_loaded_ts) >= ? \
         ORDER BY loaded_count DESC, command, session_id",
    ) else {
        return Vec::new();
    };
    // `(int(min_loads), float(min_age_days))` — the second parameter is bound
    // as a REAL on the reference, which matters to SQLite's comparison against
    // the julianday difference. Bound as `f64` here for the same reason.
    #[allow(clippy::cast_precision_loss)]
    let age = min_age_days as f64;
    let Ok(rows) = stmt.query_map(rusqlite::params![min_loads, age], |row| {
        Ok((
            row.get::<_, Option<String>>(0)?.unwrap_or_default(),
            row.get::<_, Option<String>>(1)?.unwrap_or_default(),
            row.get::<_, Option<i64>>(2)?.unwrap_or(0),
        ))
    }) else {
        return Vec::new();
    };
    rows.filter_map(std::result::Result::ok).collect()
}

/// `mark_demoted(conn, pairs)` — `UPDATE … SET demoted = 1`, rows updated.
///
/// `cursor.rowcount` after an `executemany` is the SUM of the per-statement
/// change counts, which is what the loop reproduces. `if not pairs: return 0`
/// short-circuits before the statement is even prepared, so an empty list never
/// opens a write transaction.
#[must_use]
pub fn mark_demoted(conn: &Connection, pairs: &[(String, String)]) -> i64 {
    if pairs.is_empty() {
        return 0;
    }
    let Ok(mut stmt) = conn
        .prepare("UPDATE discovery_telemetry SET demoted = 1 WHERE command = ? AND session_id = ?")
    else {
        return 0;
    };
    let mut updated: i64 = 0;
    for (command, session_id) in pairs {
        match stmt.execute(rusqlite::params![command, session_id]) {
            Ok(count) => updated += i64::try_from(count).unwrap_or(0),
            // `except sqlite3.Error: return 0` — the whole call, not this row.
            Err(_) => return 0,
        }
    }
    updated.max(0)
}

// ── the verbs ────────────────────────────────────────────────────────────────

/// `stax discovery`.
#[derive(Debug, Args)]
pub struct DiscoveryArgs {
    /// The subcommand.
    #[command(subcommand)]
    pub verb: DiscoveryVerb,
}

/// `discovery`'s two leaves.
#[derive(Debug, Subcommand)]
pub enum DiscoveryVerb {
    /// Flag sessions surfaced N+ times over M+ days that were never cited.
    ///
    /// Demoted sessions drop out of default discovery ranking (their
    /// cite-rate ranking term is zeroed) but stay reachable via direct
    /// lookup. ``--dry-run`` reports the candidates without touching them.
    #[command(name = "demote-uncited")]
    DemoteUncited(DemoteArgs),
    /// Show discovery telemetry: loaded/cited counters + cite-rate per session.
    ///
    /// ``cite_rate`` = cited_count / loaded_count (0.0 if never loaded).
    /// Rows sorted by most-recently-surfaced first.
    Telemetry(TelemetryArgs),
}

/// `discovery telemetry`.
#[derive(Debug, Args)]
pub struct TelemetryArgs {
    /// Filter to one discovery command (find_sessions_in_path | find_sessions_touching_file | search_past_decisions).
    #[arg(
        long = "command",
        value_name = "COMMAND_FILTER",
        allow_hyphen_values = true
    )]
    pub command_filter: Option<String>,
    /// Filter to one session id.
    #[arg(
        long = "session",
        value_name = "SESSION_FILTER",
        allow_hyphen_values = true
    )]
    pub session_filter: Option<String>,
    /// Max rows to show. <= 0 means no limit.
    #[arg(long = "limit", value_name = "INTEGER", default_value_t = PyInt::from(50),
          allow_hyphen_values = true, value_parser = py_int, overrides_with = "limit")]
    pub limit: PyInt,
    /// Output format.
    #[arg(long = "format", value_name = "FMT", default_value = "text",
          value_parser = ["text", "json"], overrides_with = "format")]
    pub format: String,
}

/// `discovery demote-uncited`.
#[derive(Debug, Args)]
pub struct DemoteArgs {
    /// List candidates without flagging them.
    #[arg(long = "dry-run", default_value_t = false)]
    pub dry_run: bool,
    /// Minimum times surfaced.
    #[arg(long = "min-loads", value_name = "INTEGER", default_value_t = PyInt::from(20),
          allow_hyphen_values = true, value_parser = py_int, overrides_with = "min_loads")]
    pub min_loads: PyInt,
    /// Minimum age (days since first surfaced).
    #[arg(long = "min-age-days", value_name = "INTEGER", default_value_t = PyInt::from(7),
          allow_hyphen_values = true, value_parser = py_int, overrides_with = "min_age_days")]
    pub min_age_days: PyInt,
    /// Output format.
    #[arg(long = "format", value_name = "FMT", default_value = "text",
          value_parser = ["text", "json"], overrides_with = "format")]
    pub format: String,
}

/// Run `discovery`.
///
/// # Errors
/// Only a store that cannot be opened or migrated. Every telemetry read is
/// error-swallowing by transcription, so nothing below `open_store` fails.
pub fn run_discovery(args: &DiscoveryArgs) -> Result<Output> {
    match &args.verb {
        DiscoveryVerb::DemoteUncited(args) => run_demote(args),
        DiscoveryVerb::Telemetry(args) => run_telemetry(args),
    }
}

fn run_telemetry(args: &TelemetryArgs) -> Result<Output> {
    let conn = open_store()?;
    let rows = iter_telemetry(
        &conn,
        args.command_filter.as_deref(),
        args.session_filter.as_deref(),
        args.limit.saturating_i64(),
    );
    drop(conn);

    if args.format == "json" {
        // `json.dumps({"rows": rows}, indent=2)` — NO `sort_keys`, so each row
        // keeps the DDL column order with `cite_rate` last.
        let mut payload = Map::new();
        payload.insert(
            "rows".to_owned(),
            Value::Array(rows.iter().map(TelemetryRow::to_value).collect()),
        );
        return Ok(Output::ok(format!(
            "{}\n",
            stax_reports::render::render_json(&Value::Object(payload))
        )));
    }
    Ok(Output::ok(render_telemetry_text(&rows)))
}

/// The text block `discovery telemetry` prints.
#[must_use]
pub fn render_telemetry_text(rows: &[TelemetryRow]) -> String {
    if rows.is_empty() {
        return "Discovery telemetry: no rows.\n".to_owned();
    }
    let mut out = format!("Discovery telemetry  ({} row(s))\n\n", rows.len());
    for row in rows {
        let flag = if row.demoted { "  [demoted]" } else { "" };
        out.push_str(&format!(
            "  {:<28} {}…  loaded={:<4} cited={:<4} cite_rate={:.3}{flag}\n",
            row.command,
            // `str(r['session_id'])[:12]` — twelve CHARACTERS, then a literal
            // ellipsis whether or not anything was cut.
            char_prefix(&row.session_id, 12),
            row.loaded_count,
            row.cited_count,
            row.cite_rate,
        ));
        out.push_str(&format!(
            "      first_loaded={}  last_loaded={}  last_cited={}\n",
            or_never(row.first_loaded_ts.as_deref()),
            or_never(row.last_loaded_ts.as_deref()),
            or_never(row.last_cited_ts.as_deref()),
        ));
    }
    out.push('\n');
    out
}

fn run_demote(args: &DemoteArgs) -> Result<Output> {
    let conn = open_store()?;
    let min_loads = args.min_loads.saturating_i64();
    let min_age_days = args.min_age_days.saturating_i64();
    let candidates = demote_candidates(&conn, min_loads, min_age_days);
    // `if candidates and not dry_run` — an empty candidate list never opens a
    // write, and `--dry-run` is checked here rather than inside `mark_demoted`.
    let demoted_n = if candidates.is_empty() || args.dry_run {
        0
    } else {
        let pairs: Vec<(String, String)> = candidates
            .iter()
            .map(|(command, session, _)| (command.clone(), session.clone()))
            .collect();
        mark_demoted(&conn, &pairs)
    };
    drop(conn);

    if args.format == "json" {
        let mut payload = Map::new();
        payload.insert(
            "candidates".to_owned(),
            Value::Array(
                candidates
                    .iter()
                    .map(|(command, session, loaded)| {
                        let mut row = Map::new();
                        row.insert("command".to_owned(), Value::from(command.clone()));
                        row.insert("session_id".to_owned(), Value::from(session.clone()));
                        row.insert("loaded_count".to_owned(), Value::from(*loaded));
                        Value::Object(row)
                    })
                    .collect(),
            ),
        );
        payload.insert("dry_run".to_owned(), Value::Bool(args.dry_run));
        payload.insert("demoted".to_owned(), Value::from(demoted_n));
        return Ok(Output::ok(format!(
            "{}\n",
            stax_reports::render::render_json(&Value::Object(payload))
        )));
    }
    Ok(Output::ok(render_demote_text(
        &candidates,
        args.dry_run,
        demoted_n,
        min_loads,
        min_age_days,
    )))
}

/// The text block `discovery demote-uncited` prints.
#[must_use]
pub fn render_demote_text(
    candidates: &[(String, String, i64)],
    dry_run: bool,
    demoted_n: i64,
    min_loads: i64,
    min_age_days: i64,
) -> String {
    if candidates.is_empty() {
        return format!(
            "demote-uncited: no candidates (min_loads={min_loads}, min_age_days={min_age_days}).\n"
        );
    }
    let verb = if dry_run { "Would demote" } else { "Demoted" };
    let mut out = format!("{verb} {} uncited session(s):\n\n", candidates.len());
    for (command, session, loaded) in candidates {
        out.push_str(&format!(
            "  {:<28} {}…  loaded={loaded}\n",
            command,
            char_prefix(session, 12)
        ));
    }
    out.push('\n');
    if dry_run {
        out.push_str("(dry run — nothing changed; re-run without --dry-run to apply)\n");
    } else {
        out.push_str(&format!("({demoted_n} row(s) flagged demoted)\n"));
    }
    out
}

/// `s[:n]` — CHARACTERS, which is what a Python slice counts.
fn char_prefix(text: &str, limit: usize) -> String {
    text.chars().take(limit).collect()
}

/// `value or "(never)"` — Python truthiness, so an EMPTY stamp is `(never)` too.
fn or_never(value: Option<&str>) -> &str {
    match value {
        Some(text) if !text.is_empty() => text,
        _ => "(never)",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seeded() -> Connection {
        let conn = Connection::open_in_memory().expect("store");
        conn.execute_batch(
            "CREATE TABLE discovery_telemetry (
                command TEXT NOT NULL, session_id TEXT NOT NULL,
                loaded_count INTEGER NOT NULL DEFAULT 0,
                cited_count INTEGER NOT NULL DEFAULT 0,
                first_loaded_ts TEXT, last_loaded_ts TEXT, last_cited_ts TEXT,
                demoted INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY (command, session_id));
             INSERT INTO discovery_telemetry VALUES
               ('search_past_decisions', 'aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee', 30, 0,
                '2020-01-01T00:00:00+00:00', '2026-01-03T00:00:00+00:00', NULL, 0),
               ('find_sessions_in_path', 'shortid', 30, 3,
                '2020-01-01T00:00:00+00:00', '2026-01-02T00:00:00+00:00',
                '2026-01-02T01:00:00+00:00', 0),
               ('find_sessions_touching_file', 'never-loaded', 0, 0,
                NULL, NULL, NULL, 1),
               ('find_sessions_in_path', 'too-few', 19, 0,
                '2020-01-01T00:00:00+00:00', '2026-01-01T00:00:00+00:00', NULL, 0),
               ('find_sessions_in_path', 'already-demoted', 99, 0,
                '2020-01-01T00:00:00+00:00', '2026-01-01T00:00:00+00:00', NULL, 1);",
        )
        .expect("seed");
        conn
    }

    #[test]
    fn a_missing_table_is_no_rows_and_no_error() {
        // A store predating migration v009. `except sqlite3.Error: return []`.
        let conn = Connection::open_in_memory().expect("store");
        assert!(iter_telemetry(&conn, None, None, 50).is_empty());
        assert!(demote_candidates(&conn, 20, 7).is_empty());
        assert_eq!(mark_demoted(&conn, &[("a".into(), "b".into())]), 0);
    }

    #[test]
    fn the_order_is_nulls_last_then_newest_first_then_the_two_key_columns() {
        let conn = seeded();
        let rows = iter_telemetry(&conn, None, None, 50);
        let ids: Vec<&str> = rows.iter().map(|row| row.session_id.as_str()).collect();
        assert_eq!(
            ids,
            vec![
                "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee", // 2026-01-03, newest
                "shortid",                              // 2026-01-02
                "already-demoted",                      // 2026-01-01, `a` < `t`
                "too-few",                              // 2026-01-01
                "never-loaded",                         // NULL sorts LAST
            ]
        );
    }

    #[test]
    fn an_empty_filter_string_filters_nothing() {
        // `if command:` — the `--project ''` class, twice-proven before this.
        let conn = seeded();
        assert_eq!(iter_telemetry(&conn, Some(""), Some(""), 50).len(), 5);
        assert_eq!(
            iter_telemetry(&conn, Some("find_sessions_in_path"), None, 50).len(),
            3
        );
        assert_eq!(iter_telemetry(&conn, None, Some("shortid"), 50).len(), 1);
        assert!(iter_telemetry(&conn, Some("nosuch"), None, 50).is_empty());
    }

    #[test]
    fn a_non_positive_limit_means_no_limit() {
        // `if limit and limit > 0` — both 0 and -1 fall through to no LIMIT.
        let conn = seeded();
        assert_eq!(iter_telemetry(&conn, None, None, 0).len(), 5);
        assert_eq!(iter_telemetry(&conn, None, None, -1).len(), 5);
        assert_eq!(iter_telemetry(&conn, None, None, 2).len(), 2);
    }

    #[test]
    fn cite_rate_is_zero_for_a_never_loaded_row_not_a_division_error() {
        let conn = seeded();
        let rows = iter_telemetry(&conn, None, Some("never-loaded"), 50);
        assert!((rows[0].cite_rate - 0.0).abs() < f64::EPSILON);
        assert!(rows[0].demoted);
        let rows = iter_telemetry(&conn, None, Some("shortid"), 50);
        assert!((rows[0].cite_rate - 0.1).abs() < 1e-12);
    }

    #[test]
    fn the_candidate_query_crosses_all_four_predicates() {
        let conn = seeded();
        let candidates = demote_candidates(&conn, 20, 7);
        let ids: Vec<&str> = candidates
            .iter()
            .map(|(_, session, _)| session.as_str())
            .collect();
        assert_eq!(
            ids,
            vec!["aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee"],
            "cited (shortid), too few loads (too-few), already demoted \
             (already-demoted) and a NULL first_loaded_ts (never-loaded) are all out"
        );
        // Lower the load floor and `too-few` joins, worst offender first.
        let candidates = demote_candidates(&conn, 19, 7);
        assert_eq!(candidates[0].2, 30);
        assert_eq!(candidates[1].2, 19);
    }

    #[test]
    fn mark_demoted_sums_the_per_statement_change_counts() {
        let conn = seeded();
        let pairs = vec![
            (
                "search_past_decisions".to_owned(),
                "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".to_owned(),
            ),
            ("find_sessions_in_path".to_owned(), "nosuchrow".to_owned()),
        ];
        assert_eq!(mark_demoted(&conn, &pairs), 1, "the miss contributes 0");
        assert!(demote_candidates(&conn, 20, 7).is_empty(), "and it stuck");
        assert_eq!(mark_demoted(&conn, &[]), 0);
    }

    #[test]
    fn the_text_block_pads_the_command_and_cuts_the_session_at_twelve_characters() {
        let conn = seeded();
        let rows = iter_telemetry(&conn, None, None, 2);
        let text = render_telemetry_text(&rows);
        assert_eq!(
            text,
            "Discovery telemetry  (2 row(s))\n\
             \n\
             \x20 search_past_decisions        aaaaaaaa-bbb…  loaded=30   cited=0    cite_rate=0.000\n\
             \x20     first_loaded=2020-01-01T00:00:00+00:00  last_loaded=2026-01-03T00:00:00+00:00  last_cited=(never)\n\
             \x20 find_sessions_in_path        shortid…  loaded=30   cited=3    cite_rate=0.100\n\
             \x20     first_loaded=2020-01-01T00:00:00+00:00  last_loaded=2026-01-02T00:00:00+00:00  last_cited=2026-01-02T01:00:00+00:00\n\
             \n"
        );
    }

    #[test]
    fn the_demoted_flag_and_the_empty_block() {
        let conn = seeded();
        let rows = iter_telemetry(&conn, None, Some("never-loaded"), 50);
        assert!(render_telemetry_text(&rows).contains("  [demoted]\n"));
        assert_eq!(
            render_telemetry_text(&[]),
            "Discovery telemetry: no rows.\n"
        );
    }

    #[test]
    fn the_demote_block_says_would_on_a_dry_run_and_names_both_thresholds_when_empty() {
        assert_eq!(
            render_demote_text(&[], true, 0, 20, 7),
            "demote-uncited: no candidates (min_loads=20, min_age_days=7).\n"
        );
        let candidates = vec![(
            "search_past_decisions".to_owned(),
            "abcdefghijklmnop".to_owned(),
            30,
        )];
        assert_eq!(
            render_demote_text(&candidates, true, 0, 20, 7),
            "Would demote 1 uncited session(s):\n\
             \n\
             \x20 search_past_decisions        abcdefghijkl…  loaded=30\n\
             \n\
             (dry run — nothing changed; re-run without --dry-run to apply)\n"
        );
        assert!(
            render_demote_text(&candidates, false, 1, 20, 7)
                .starts_with("Demoted 1 uncited session(s):\n"),
        );
        assert!(
            render_demote_text(&candidates, false, 1, 20, 7)
                .ends_with("(1 row(s) flagged demoted)\n")
        );
    }
}
