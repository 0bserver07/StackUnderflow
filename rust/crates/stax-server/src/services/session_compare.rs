//! `routes/sessions.py::compare_sessions` — the two-session diff (RS-5-105).
//!
//! | Item | Python | Rust |
//! |---|---|---|
//! | `_session_costs_for_sessions` | rebuilds `RawEntry` from two sessions' `raw_json` | [`session_costs_for_sessions`] |
//! | the session-id resolution + its 404 | inline in the handler | [`resolve_sessions`], [`missing_sessions`], [`not_found_detail`] |
//! | the `diff` block | inline in the handler | [`build_diff`] |
//! | the whole store-side body | inline in the handler | [`compare_payload`] |
//!
//! # Why this is a service module and not twenty lines in the route
//!
//! Not because a CLI verb shares it — none does. Because the endpoint's body is
//! a *pipeline* invocation: it reconstructs `classifier::RawEntry` values out of
//! `messages.raw_json`, runs `tag` → `build_detailed` →
//! `aggregator::summarise_session_costs`, and then does float arithmetic on the
//! result. `routes/sessions.rs` is otherwise pure SQL-to-JSON, and mixing the
//! two shapes in one file is how the pipeline call site gets copied the next
//! time somebody needs it. The store-shape helpers the route already owns and
//! shares with `/api/jsonl-files` and `/api/jsonl-content`
//! (`get_projects_by_slug`, `ProjectRow`, the slug 404) stay there; this module
//! takes their *output*.
//!
//! # The narrowing that makes the endpoint affordable
//!
//! The Cost tab's `session_costs` section normally comes out of the
//! whole-project pipeline: every message materialised, enriched and aggregated
//! (~3.4 s on a large project) to diff two rows. Python narrows it twice and
//! both narrowings are ported verbatim:
//!
//! * **Only the session-cost collector runs.** `summarise_session_costs` is the
//!   single-section entry point batch E added to `stax_etl::stats::aggregator`;
//!   every field of `_SessionCostCollector` is keyed by `session_id` and the
//!   `commands` tally is a per-session interaction count, so restricting the
//!   dataset to `a` and `b` yields the same rows for them.
//! * **The message fetch is UNORDERED.** Python's SQL has no `ORDER BY`:
//!   `enricher::build` sorts by timestamp itself when it groups interactions,
//!   and the collector is keyed by session. Adding an `ORDER BY` here would be
//!   a "fix" that changes the answer — `by_model` is insertion-ordered and the
//!   cost is a `+=` chain over it, so row order moves the last bits of `cost`.
//!   Same statement, same store, same order.
//!
//! # `log_dir` is computed by Python and read by nobody
//!
//! `resolve_legacy_log_dir(...)` feeds `enricher.build(tagged, log_dir)`, which
//! passes it to step 5 (`scan_sessions`) — the step `stax_etl::stats::enricher`
//! does not port, because nothing reads `EnrichedDataset.sessions`. It is not
//! computed here. The call is pure (an env read plus `Path.home()`), raises
//! nothing a request can observe, and its result never reaches the response.
//!
//! # Divergences this module files
//!
//! * **The `diff.tokens` key order is not reproducible, in Python or here.**
//!   `keys = set(sa["tokens"]) | set(sb["tokens"])`, and the dict comprehension
//!   iterates that `set`. CPython randomises `str` hashing per process unless
//!   `PYTHONHASHSEED` is pinned, and `endpoint-parity.sh` does not pin it
//!   (`parity-cli.sh` does), so the reference server emits a *different* key
//!   order on every boot. Measured: three runs of the reference handler over
//!   the harness store gave `cache_read,cache_creation,input,output`, then
//!   `input,cache_read,cache_creation,output`, then
//!   `input,cache_creation,cache_read,output`. Every other byte of the payload
//!   was identical across all three. See [`token_diff`] for the deterministic
//!   order chosen here and `parity/DIV-e-compare.md` for the full note.
//! * **An unparseable `raw_json` is skipped, not raised.** Python's
//!   `_json.loads` has no `try`, so a poison blob becomes a 500 whose `detail`
//!   is CPython's `JSONDecodeError` text. The same choice `routes/data.rs` and
//!   `stats::dataset` already made (DIV-064) is made here: skip the row. A store
//!   holding one is a store Python cannot serve at all.

use std::collections::{HashMap, HashSet};

use rusqlite::Connection;
use serde_json::{Map, Value};
use stax_etl::pricing::PricingEngine;
use stax_etl::stats::aggregator::{jf, ji, summarise_session_costs};
use stax_etl::stats::classifier::{RawEntry, tag};
use stax_etl::stats::enricher::build_detailed;

/// One row of `SELECT id, session_id, project_id FROM sessions …`.
#[derive(Debug, Clone)]
pub struct SessionRef {
    /// The session's integer PK — `messages.session_fk`.
    pub id: i64,
    /// The public session id the caller passed as `a` / `b`.
    pub session_id: String,
    /// The owning project, used only to look up the pricing provider.
    pub project_id: i64,
}

/// How the store-side body of the handler ends.
///
/// Python has three exits: a re-raised `HTTPException` (the two 404s), the
/// `except Exception` funnel (`500 "Failed to load stats: {e}"`), and success.
#[derive(Debug)]
pub enum CompareError {
    /// `HTTPException(404, …)` — an unknown session id.
    NotFound(String),
    /// `except Exception as e: raise HTTPException(500, f"Failed to load stats: {e}")`.
    Failed(String),
}

impl From<rusqlite::Error> for CompareError {
    fn from(err: rusqlite::Error) -> Self {
        Self::Failed(err.to_string())
    }
}

/// `",".join("?" for _ in xs)`.
///
/// File-local, as it is in `services/mart_queries.rs` and
/// `services/yield_tracker.rs` — there is no shared owner for this in the
/// server crate, and inventing one would touch files this batch may not.
fn placeholders(count: usize) -> String {
    std::iter::repeat_n("?", count)
        .collect::<Vec<_>>()
        .join(",")
}

// ── the session-id resolution and its 404 ───────────────────────────────────

/// The handler's up-front session lookup.
///
/// ```sql
/// SELECT id, session_id, project_id FROM sessions
///  WHERE project_id IN (?, …) AND session_id IN (?, ?)
/// ```
///
/// The `IN (?, ?)` is ALWAYS two placeholders, both bound, even when `a == b` —
/// SQLite collapses the duplicate and returns one row, which is exactly what
/// Python gets and why the `a == b` case answers `200` with a zero diff instead
/// of a 404.
///
/// A missing id 404s here, before a single message is read.
///
/// # Errors
/// On a SQLite failure.
pub fn resolve_sessions(
    conn: &Connection,
    project_ids: &[i64],
    a: &str,
    b: &str,
) -> rusqlite::Result<Vec<SessionRef>> {
    let sql = format!(
        "SELECT id, session_id, project_id FROM sessions \
         WHERE project_id IN ({}) AND session_id IN (?, ?)",
        placeholders(project_ids.len())
    );
    let mut stmt = conn.prepare(&sql)?;
    let mut params: Vec<&dyn rusqlite::ToSql> = project_ids
        .iter()
        .map(|id| id as &dyn rusqlite::ToSql)
        .collect();
    params.push(&a);
    params.push(&b);
    stmt.query_map(params.as_slice(), |row| {
        Ok(SessionRef {
            id: row.get(0)?,
            session_id: row.get(1)?,
            project_id: row.get(2)?,
        })
    })?
    .collect()
}

/// `[sid for sid in (a, b) if sid not in found_sids]`.
///
/// The tuple is `(a, b)` and the guard is membership, not deduplication, so
/// `a == b` on an unknown id yields the id **twice** — the 404 really does read
/// `Session(s) not found: zzz, zzz`. Measured against the reference.
#[must_use]
pub fn missing_sessions(a: &str, b: &str, found: &[SessionRef]) -> Vec<String> {
    let found: HashSet<&str> = found.iter().map(|row| row.session_id.as_str()).collect();
    [a, b]
        .into_iter()
        .filter(|sid| !found.contains(sid))
        .map(str::to_owned)
        .collect()
}

/// `f"Session(s) not found: {', '.join(missing)}"`.
#[must_use]
pub fn not_found_detail(missing: &[String]) -> String {
    format!("Session(s) not found: {}", missing.join(", "))
}

// ── _session_costs_for_sessions ─────────────────────────────────────────────

/// `_session_costs_for_sessions` — the `session_costs` rows for `sess_rows`.
///
/// Reconstructs pipeline `RawEntry` values from just these sessions' `raw_json`
/// (driven off `session_fk`, so the partitioned `messages` view stays on its
/// per-partition index), then runs `classifier::tag` →
/// `enricher::build_detailed` → `aggregator::summarise_session_costs`.
///
/// `classifier::tag` and the enricher are not skippable: the collector's
/// `commands` tally comes from `ds.interactions`, which only the enricher
/// produces.
///
/// Python's `RawEntry` carries a fourth field, `origin=sid`. The Rust `RawEntry`
/// has never had it — nothing downstream of `classifier.tag` reads `origin` — so
/// there is nothing to set.
///
/// # Errors
/// On a SQLite failure.
pub fn session_costs_for_sessions(
    conn: &Connection,
    sess_rows: &[SessionRef],
    provider_map: &HashMap<i64, String>,
    engine: &PricingEngine,
) -> rusqlite::Result<Value> {
    // `fk_to_sid` / `fk_to_provider` are dicts keyed by the session PK, and
    // `fks = list(fk_to_sid)` is their INSERTION order — `sess_rows` order,
    // which is SQL row order.
    let mut fks: Vec<i64> = Vec::with_capacity(sess_rows.len());
    let mut fk_to_sid: HashMap<i64, &str> = HashMap::with_capacity(sess_rows.len());
    let mut fk_to_provider: HashMap<i64, &str> = HashMap::with_capacity(sess_rows.len());
    for row in sess_rows {
        if fk_to_sid.insert(row.id, row.session_id.as_str()).is_none() {
            fks.push(row.id);
        }
        fk_to_provider.insert(
            row.id,
            provider_map
                .get(&row.project_id)
                .map_or("anthropic", String::as_str),
        );
    }
    // `if not fks: return []`.
    if fks.is_empty() {
        return Ok(Value::Array(Vec::new()));
    }

    // NO `ORDER BY` — see the module docs. The statement is Python's, verbatim.
    let sql = format!(
        "SELECT session_fk, raw_json, timestamp FROM messages \
         WHERE session_fk IN ({})",
        placeholders(fks.len())
    );
    let mut raw_entries: Vec<RawEntry> = Vec::new();
    {
        let mut stmt = conn.prepare(&sql)?;
        let mut rows = stmt.query(rusqlite::params_from_iter(fks.iter()))?;
        while let Some(row) = rows.next()? {
            let session_fk: i64 = row.get(0)?;
            let raw_json: Option<String> = row.get(1)?;
            let timestamp: Option<String> = row.get(2)?;

            // `sid = fk_to_sid.get(r["session_fk"], "")` — a `messages` row
            // whose `session_fk` is not in the map takes the EMPTY session id
            // rather than being dropped. Unreachable through this statement;
            // ported anyway.
            let sid = fk_to_sid.get(&session_fk).copied().unwrap_or("");
            // DIV: `_json.loads` has no `try` on the Python side. See the module
            // docs — skipping matches `routes/data.rs` and DIV-064.
            let Some(mut payload) = raw_json
                .as_deref()
                .and_then(|text| serde_json::from_str::<Value>(text).ok())
            else {
                continue;
            };
            // The authoritative clean timestamp lives in the column; `raw_json`
            // may hold epoch-millis ints from non-Claude adapters.
            // `if r["timestamp"]:` is a truthiness check, so an empty string
            // leaves the payload alone.
            if let Some(ts) = timestamp.filter(|ts| !ts.is_empty())
                && let Some(obj) = payload.as_object_mut()
            {
                obj.insert("timestamp".to_owned(), Value::String(ts));
            }
            raw_entries.push(RawEntry {
                payload,
                session_id: sid.to_owned(),
                provider: fk_to_provider
                    .get(&session_fk)
                    .copied()
                    .unwrap_or("anthropic")
                    .to_owned(),
            });
        }
    }

    // `if not raw_entries: return []`.
    if raw_entries.is_empty() {
        return Ok(Value::Array(Vec::new()));
    }
    let dataset = build_detailed(tag(raw_entries));
    Ok(summarise_session_costs(&dataset, engine))
}

// ── the diff ────────────────────────────────────────────────────────────────

/// `sb["tokens"].get(k, 0) - sa["tokens"].get(k, 0)` for every key of the union.
///
/// **The key ORDER is a divergence and cannot be otherwise.** Python iterates
/// `set(sa["tokens"]) | set(sb["tokens"])`, whose order is CPython's hash-table
/// slot order over hash-randomised `str`s — different on every process start.
/// The order chosen here is the only deterministic one with a defensible
/// derivation: `a`'s keys in their own insertion order, then `b`'s keys that `a`
/// did not have, in theirs. On the live shape (`_usage_from` always writes the
/// same four keys, `reasoning` appended after them when > 0) that is
/// `input, output, cache_creation, cache_read[, reasoning]`, which is also the
/// order both `tokens` objects in the same response already use — so the
/// response reads consistently even though it cannot read *identically*.
///
/// The values are Python `int`s on both sides: `Counter` holds ints and a
/// missing key defaults to a literal `0`, so no subtraction here can produce a
/// float.
#[must_use]
pub fn token_diff(sa: &Value, sb: &Value) -> Value {
    // `sa.get("tokens", {})` — a row without the key contributes nothing.
    let empty = Map::new();
    let ta = sa
        .get("tokens")
        .and_then(Value::as_object)
        .unwrap_or(&empty);
    let tb = sb
        .get("tokens")
        .and_then(Value::as_object)
        .unwrap_or(&empty);

    let mut out = Map::new();
    for key in ta.keys().chain(tb.keys()) {
        if out.contains_key(key) {
            continue;
        }
        let left = ta.get(key).and_then(Value::as_i64).unwrap_or(0);
        let right = tb.get(key).and_then(Value::as_i64).unwrap_or(0);
        out.insert(key.clone(), ji(right - left));
    }
    Value::Object(out)
}

/// The `diff` block, key for key in Python's order.
///
/// `cost` and `duration_s` are float − float and stay floats even at zero
/// (`0.0`, not `0`); `commands` and `errors` are int − int. That split is four
/// bytes on the wire.
#[must_use]
pub fn build_diff(sa: &Value, sb: &Value) -> Value {
    let num = |row: &Value, key: &str| row.get(key).and_then(Value::as_f64).unwrap_or(0.0);
    let int = |row: &Value, key: &str| row.get(key).and_then(Value::as_i64).unwrap_or(0);

    let mut diff = Map::new();
    diff.insert("cost".to_owned(), jf(num(sb, "cost") - num(sa, "cost")));
    diff.insert("tokens".to_owned(), token_diff(sa, sb));
    diff.insert(
        "commands".to_owned(),
        ji(int(sb, "commands") - int(sa, "commands")),
    );
    diff.insert(
        "errors".to_owned(),
        ji(int(sb, "errors") - int(sa, "errors")),
    );
    diff.insert(
        "duration_s".to_owned(),
        jf(num(sb, "duration_s") - num(sa, "duration_s")),
    );
    Value::Object(diff)
}

// ── the whole store-side body ───────────────────────────────────────────────

/// Everything the handler does between `db.connect` and the currency stamp.
///
/// Returns `{"a": …, "b": …, "diff": …}` in that key order; the route inserts
/// `currency` last and applies the conversion rate, because Python reads the
/// currency payload *outside* the `try` that produces the 500.
///
/// The second 404 (`sa is None or sb is None`) is not dead code even though the
/// first one already proved both ids exist as `sessions` rows: a session with no
/// surviving `messages` row produces no `session_costs` entry, and Python
/// answers that with the same message.
///
/// # Errors
/// [`CompareError::NotFound`] for either 404; [`CompareError::Failed`] for the
/// `except Exception` funnel.
pub fn compare_payload(
    conn: &Connection,
    engine: &PricingEngine,
    project_ids: &[i64],
    provider_map: &HashMap<i64, String>,
    a: &str,
    b: &str,
) -> Result<Value, CompareError> {
    let sess_rows = resolve_sessions(conn, project_ids, a, b)?;
    let missing = missing_sessions(a, b, &sess_rows);
    if !missing.is_empty() {
        return Err(CompareError::NotFound(not_found_detail(&missing)));
    }

    let session_costs = session_costs_for_sessions(conn, &sess_rows, provider_map, engine)?;

    // `by_id = {s["session_id"]: s for s in session_costs}` — a dict, so a
    // duplicated session id would keep the LAST row. The collector is keyed by
    // session id, so it cannot emit one.
    let mut by_id: HashMap<&str, &Value> = HashMap::new();
    if let Some(rows) = session_costs.as_array() {
        for row in rows {
            if let Some(sid) = row.get("session_id").and_then(Value::as_str) {
                by_id.insert(sid, row);
            }
        }
    }
    let sa = by_id.get(a).copied();
    let sb = by_id.get(b).copied();
    let (Some(sa), Some(sb)) = (sa, sb) else {
        // `[sid for sid, hit in ((a, sa), (b, sb)) if hit is None]` — same
        // duplicate-spelling behaviour as the first 404 when `a == b`.
        let missing: Vec<String> = [(a, sa), (b, sb)]
            .into_iter()
            .filter(|(_, hit)| hit.is_none())
            .map(|(sid, _)| sid.to_owned())
            .collect();
        return Err(CompareError::NotFound(not_found_detail(&missing)));
    };

    let diff = build_diff(sa, sb);
    let mut out = Map::new();
    out.insert("a".to_owned(), sa.clone());
    out.insert("b".to_owned(), sb.clone());
    out.insert("diff".to_owned(), diff);
    Ok(Value::Object(out))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn row(session_id: &str) -> SessionRef {
        SessionRef {
            id: 1,
            session_id: session_id.to_owned(),
            project_id: 1,
        }
    }

    #[test]
    fn a_missing_id_is_named_once_per_position_not_once_per_value() {
        // `?a=zzz&b=zzz` on an unknown id lists it TWICE — the comprehension
        // walks the tuple `(a, b)`, it does not deduplicate. Measured against
        // the reference handler.
        assert_eq!(
            not_found_detail(&missing_sessions("zzz", "zzz", &[])),
            "Session(s) not found: zzz, zzz"
        );
        assert_eq!(
            not_found_detail(&missing_sessions("x", "y", &[row("x")])),
            "Session(s) not found: y"
        );
        assert!(missing_sessions("x", "y", &[row("x"), row("y")]).is_empty());
        // An empty `?a=` is a perfectly valid `str` to FastAPI; it just names no
        // session, so it reaches this 404 with an empty spelling.
        assert_eq!(
            not_found_detail(&missing_sessions("", "y", &[row("y")])),
            "Session(s) not found: "
        );
    }

    #[test]
    fn the_in_clause_binds_two_ids_even_when_they_are_equal() {
        assert_eq!(
            format!(
                "WHERE project_id IN ({}) AND session_id IN (?, ?)",
                placeholders(3)
            ),
            "WHERE project_id IN (?,?,?) AND session_id IN (?, ?)"
        );
    }

    #[test]
    fn the_diff_keeps_pythons_key_order_and_its_int_float_split() {
        let sa = json!({"session_id": "a", "duration_s": 1.5, "cost": 2.0,
                        "tokens": {"input": 1, "output": 2}, "commands": 3, "errors": 1});
        let sb = json!({"session_id": "b", "duration_s": 4.0, "cost": 5.0,
                        "tokens": {"input": 10, "output": 20}, "commands": 8, "errors": 1});
        assert_eq!(
            stax_memory::pyjson::dumps_http(&build_diff(&sa, &sb)),
            r#"{"cost":3.0,"tokens":{"input":9,"output":18},"commands":5,"errors":0,"duration_s":2.5}"#
        );
    }

    #[test]
    fn a_zero_diff_still_writes_floats_where_python_writes_floats() {
        // `a == b`: every difference is zero, and `cost` / `duration_s` must
        // still render `0.0` while `commands` / `errors` render `0`.
        let row = json!({"session_id": "a", "duration_s": 0.0, "cost": 0.0,
                         "tokens": {"input": 0, "output": 0}, "commands": 0, "errors": 0});
        assert_eq!(
            stax_memory::pyjson::dumps_http(&build_diff(&row, &row)),
            r#"{"cost":0.0,"tokens":{"input":0,"output":0},"commands":0,"errors":0,"duration_s":0.0}"#
        );
    }

    #[test]
    fn the_token_union_takes_as_keys_first_then_bs_extras() {
        // The deterministic stand-in for Python's `set` union. `reasoning` is
        // appended after the four fixed keys by `_parse_entry`, so a session
        // that has it and one that does not still read in the same order.
        let sa = json!({"tokens": {"input": 1, "output": 2, "cache_creation": 0, "cache_read": 0}});
        let sb = json!({"tokens": {"input": 5, "output": 9, "cache_creation": 0,
                                   "cache_read": 0, "reasoning": 7}});
        assert_eq!(
            stax_memory::pyjson::dumps_http(&token_diff(&sa, &sb)),
            r#"{"input":4,"output":7,"cache_creation":0,"cache_read":0,"reasoning":7}"#
        );
        // …and a key only `a` has still appears, with a NEGATIVE value.
        assert_eq!(
            stax_memory::pyjson::dumps_http(&token_diff(&sb, &sa)),
            r#"{"input":-4,"output":-7,"cache_creation":0,"cache_read":0,"reasoning":-7}"#
        );
    }

    #[test]
    fn a_row_with_no_tokens_key_diffs_to_an_empty_object() {
        // `sa.get("tokens", {})` — the `.get` with a default is the only reason
        // this is not an exception on either side.
        assert_eq!(token_diff(&json!({}), &json!({})), json!({}));
    }
}
