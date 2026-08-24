//! `services/risk.py` (179 ln) — the per-file revert/failure overlay.
//!
//! # This module is an ADAPTER, and that is the whole point
//!
//! `file_risk_summary` was already ported, in full, as
//! [`stax_core::queries::file_risk_summary`] — because the CLI reaches it too
//! (`stax memory file <path>` renders the same four counts through
//! `stax-cli`'s `memory.rs`). The four heuristic pieces it stands on —
//! `discovery.parse_since`, `_resolve_input_path`, `find_failure_modes_for_file`
//! and `_outcome_matches_for`, roughly 400 lines of the 2,482-line
//! `services/discovery.py` — live in `stax-core` beside it.
//!
//! LAW 9 says use the deduped owner and do not re-create a file-local copy. So
//! this file is ~40 lines of glue and not a second port: it calls that
//! function, applies the route's threshold, and renders the four keys the
//! `/fs` endpoint decorates a file with. A transliteration here would have
//! forked the outcome ladder, and DIV-035 already priced what a second copy of
//! a shared routine costs (145 false divergences from one re-written
//! formatter).
//!
//! # What the route asks for, and what it does with the answer
//!
//! ```text
//! summary = risk_service.file_risk_summary(conn, path)     # since=None, recent_limit=5
//! if summary["reverted"] > 0 or summary["failed"] > 0:
//!     files[path]["risk"] = {"reverted_count", "failed_count",
//!                            "worked_count", "total_sessions"}
//! ```
//!
//! Three consequences worth naming:
//!
//! * the threshold is `reverted > 0 or failed > 0` — a file with `worked: 9`
//!   and no failures gets **no** `risk` key at all, which is why the metadata
//!   fetch stays small;
//! * `recent_session_ids` and `path` and `since` are computed and **discarded**
//!   — the overlay names four keys and those are not among them. The work is
//!   still done, because the reference does it;
//! * the renamed keys (`reverted` → `reverted_count`, …) are the overlay's, in
//!   the dict-literal's order, and `total_sessions` keeps its name.

use rusqlite::Connection;
use serde_json::{Map, Value};

/// The four counts the `/fs` overlay reads, in the reference's key order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RiskOverlay {
    /// `summary["reverted"]`.
    pub reverted: i64,
    /// `summary["failed"]`.
    pub failed: i64,
    /// `summary["worked"]`.
    pub worked: i64,
    /// `summary["total_sessions"]`.
    pub total_sessions: i64,
}

impl RiskOverlay {
    /// `if summary["reverted"] > 0 or summary["failed"] > 0` — the gate that
    /// decides whether a `risk` key appears on the file entry at all.
    #[must_use]
    pub const fn is_noteworthy(self) -> bool {
        self.reverted > 0 || self.failed > 0
    }

    /// The `files[path]["risk"]` object, in the dict-literal's order.
    #[must_use]
    pub fn to_value(self) -> Value {
        let mut obj = Map::new();
        obj.insert("reverted_count".to_owned(), Value::from(self.reverted));
        obj.insert("failed_count".to_owned(), Value::from(self.failed));
        obj.insert("worked_count".to_owned(), Value::from(self.worked));
        obj.insert(
            "total_sessions".to_owned(),
            Value::from(self.total_sessions),
        );
        Value::Object(obj)
    }
}

/// `risk_service.file_risk_summary(conn, path)` — the route's call, exactly.
///
/// `since=None` and `recent_limit=5` are the reference's defaults and the route
/// passes neither, so they are baked in here rather than plumbed: a caller that
/// wants the windowed form is the CLI, and the CLI already calls
/// [`stax_core::queries::file_risk_summary`] directly.
///
/// `None` is the route's `except (ValueError, sqlite3.DatabaseError): continue`
/// — a malformed path or a flaky read must not fail the snapshot endpoint.
///
/// **Widening (recorded, and it cannot fire).** The reference catches exactly
/// two exception types; this swallows every `anyhow::Error` the query can
/// produce. With `since = None` the only `ValueError` source (`parse_since`) is
/// unreachable, and every remaining failure inside `file_risk_summary` is a
/// `rusqlite::Error`, which is `sqlite3.DatabaseError`'s counterpart. So the two
/// catch sets are the same set on this call path.
#[must_use]
pub fn file_risk_overlay(conn: &Connection, path: &str) -> Option<RiskOverlay> {
    let summary = stax_core::queries::file_risk_summary(conn, path, None, 5).ok()?;
    Some(RiskOverlay {
        reverted: summary.reverted,
        failed: summary.failed,
        worked: summary.worked,
        total_sessions: summary.total_sessions,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_overlay_renders_the_four_renamed_keys_in_order() {
        let overlay = RiskOverlay {
            reverted: 2,
            failed: 1,
            worked: 3,
            total_sessions: 7,
        };
        assert_eq!(
            stax_memory::pyjson::dumps_http(&overlay.to_value()),
            r#"{"reverted_count":2,"failed_count":1,"worked_count":3,"total_sessions":7}"#
        );
    }

    /// The gate is `reverted > 0 or failed > 0` — `worked` and
    /// `total_sessions` do NOT open it, however large they are.
    #[test]
    fn a_file_with_only_successes_gets_no_risk_key() {
        let clean = RiskOverlay {
            reverted: 0,
            failed: 0,
            worked: 99,
            total_sessions: 120,
        };
        assert!(!clean.is_noteworthy());
        assert!(
            RiskOverlay {
                reverted: 1,
                ..clean
            }
            .is_noteworthy()
        );
        assert!(RiskOverlay { failed: 1, ..clean }.is_noteworthy());
    }

    /// A store with none of the discovery tables must yield `None`, not a
    /// propagated error — the route's `continue`.
    #[test]
    fn a_store_without_the_discovery_tables_is_a_swallowed_miss() {
        let conn = Connection::open_in_memory().expect("open");
        assert_eq!(file_risk_overlay(&conn, "/repo/a.py"), None);
    }

    #[test]
    fn a_file_nothing_ever_touched_is_all_zeroes_and_not_noteworthy() {
        let conn = Connection::open_in_memory().expect("open");
        conn.execute_batch(
            "CREATE TABLE sessions (id INTEGER PRIMARY KEY, project_id INTEGER,
                 session_id TEXT, first_ts TEXT, last_ts TEXT, message_count INTEGER);
             CREATE TABLE messages (id INTEGER PRIMARY KEY, session_fk INTEGER,
                 seq INTEGER, timestamp TEXT, role TEXT,
                 content_text TEXT DEFAULT '', tools_json TEXT DEFAULT '[]');",
        )
        .expect("schema");
        let overlay = file_risk_overlay(&conn, "/repo/never-seen.py").expect("summary");
        assert_eq!(
            overlay,
            RiskOverlay {
                reverted: 0,
                failed: 0,
                worked: 0,
                total_sessions: 0,
            }
        );
        assert!(!overlay.is_noteworthy());
    }
}
