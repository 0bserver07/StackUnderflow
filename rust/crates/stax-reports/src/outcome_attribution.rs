//! `services/outcome_attribution.py` — sessions ↔ commits ↔ PRs ↔ CI runs.
//!
//! | Item | Python | Rust |
//! |---|---|---|
//! | `get_outcomes_for_session(conn, session_id)` | 189-257 | [`get_outcomes_for_session`] |
//! | `_pr_matches_commit(raw_json, sha)` | 158-186 | [`pr_matches_commit`] |
//!
//! `routes/yield_route.py` imports exactly the first of those, and calls it once
//! per yield entry to stamp `pr` and `ci_runs` onto the row.
//!
//! # What is deliberately absent
//!
//! `link_commits_to_sessions` — the module's other half — is **not ported, and
//! must not be** (DIV-099(b)). It is the post-ingest hook: it shells out to
//! `git log` over every unlinked session and then `INSERT OR IGNORE INTO
//! commit_session_link … ; conn.commit()`. It is a **writer**, it is on no
//! route's path, and a writer one call away from a parity case row is exactly
//! the shape of DIV-059 and DIV-078. Its three private helpers (`parse_iso_ts`,
//! `get_session_cwd`, `get_git_repo_slug`) serve only it and are skipped with
//! it. This module reads three tables and writes nothing.
//!
//! # What is load-bearing
//!
//! * **The dedup keeps the FIRST position and the LAST value.** `unique_prs[key]
//!   = pr` over a repeated key updates the value in place, and CPython's dict
//!   does not move an existing key to the end. A `HashMap` would randomise the
//!   order outright, and a naive "remove then insert" would move it. Both are
//!   payload divergences; [`OrderedDedup`] is neither.
//! * **The PR scan is a `LIKE '%sha%'` prefilter followed by a JSON re-check.**
//!   The SQL can match a sha mentioned anywhere in the blob — a base sha, a
//!   parent, a body quote — and [`pr_matches_commit`] is what narrows it to the
//!   four fields that actually mean "this PR carries this commit".
//! * **A `raw_json` that parses to something other than an object is a `500`.**
//!   Only `json.loads` sits inside Python's `try`; the very next line is
//!   `data.get(...)`, which raises `AttributeError` on a list or a scalar.
//!   Ported as [`OutcomeError::NonObjectPayload`] rather than silently answering
//!   `False`.

use std::collections::HashMap;

use rusqlite::Connection;
use rusqlite::types::Value as SqlValue;
use serde_json::{Map, Value};

/// The three lists `get_outcomes_for_session` returns.
///
/// Python hands back `{"commits": …, "prs": …, "ci_runs": …}`; the route only
/// reads two of them, but `commits` is built either way and is the loop's input,
/// so it is carried rather than dropped.
#[derive(Debug, Clone, Default)]
pub struct Outcomes {
    /// `[dict(c) for c in commits]` — keys `commit_sha`, `repo_slug`,
    /// `committed_at`, in SELECT order.
    pub commits: Vec<Value>,
    /// Deduplicated by `(provider, repo_slug, pr_number)`.
    pub prs: Vec<Value>,
    /// Deduplicated by `(provider, run_id)`.
    pub ci_runs: Vec<Value>,
}

/// What [`get_outcomes_for_session`] can fail with.
#[derive(Debug)]
pub enum OutcomeError {
    /// A SQLite failure — including "no such table" on a store predating the
    /// outcome-attribution schema, which Python also lets escape as a 500.
    Sql(rusqlite::Error),
    /// `_pr_matches_commit`'s `data.get("pull_request", data)` on a `raw_json`
    /// that decoded to a list, a string or a number: `AttributeError`, outside
    /// the `try`, therefore a 500.
    NonObjectPayload,
}

impl std::fmt::Display for OutcomeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Sql(err) => write!(f, "{err}"),
            Self::NonObjectPayload => write!(f, "'list' object has no attribute 'get'"),
        }
    }
}

impl std::error::Error for OutcomeError {}

impl From<rusqlite::Error> for OutcomeError {
    fn from(err: rusqlite::Error) -> Self {
        Self::Sql(err)
    }
}

/// `get_outcomes_for_session(conn, session_id)`.
///
/// One query for the session's commits, then **two more per commit** — the PR
/// `LIKE` scan and the CI-run lookup. That is N+1 by construction, and the route
/// calls this once per yield entry on top, so the whole shape is N×M. It is
/// ported as written: batching it into a single `IN (…)` query would change the
/// row order inside `prs` and `ci_runs`, and the order is the payload.
///
/// # Errors
/// SQLite failure, or a `pr_outcomes.raw_json` that decodes to a non-object.
pub fn get_outcomes_for_session(
    conn: &Connection,
    session_id: &str,
) -> Result<Outcomes, OutcomeError> {
    // No ORDER BY in the Python, so the row order is whatever the scan yields —
    // stable for a given database file, which is all the differ needs.
    let mut stmt = conn.prepare(
        "SELECT commit_sha, repo_slug, committed_at FROM commit_session_link WHERE session_id = ?",
    )?;
    let commits = stmt
        .query_map([session_id], |row| {
            Ok((
                row.get::<_, SqlValue>(0)?,
                row.get::<_, SqlValue>(1)?,
                row.get::<_, SqlValue>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let mut commit_dicts = Vec::with_capacity(commits.len());
    let mut prs = OrderedDedup::default();
    let mut cis = OrderedDedup::default();

    for (commit_sha, repo_slug, committed_at) in commits {
        commit_dicts.push(object(&[
            ("commit_sha", commit_sha.clone()),
            ("repo_slug", repo_slug),
            ("committed_at", committed_at),
        ]));

        // `c["commit_sha"]` is used both as the LIKE needle and as the equality
        // target below. A non-text sha would make the `f"%{sha}%"` pattern
        // Python's `str()` of it; every writer stores TEXT.
        let sha = match &commit_sha {
            SqlValue::Text(text) => text.clone(),
            other => py_str(other),
        };

        for candidate in pr_candidates(conn, &sha)? {
            // `if _pr_matches_commit(cand["raw_json"], sha)` — the JSON re-check
            // that narrows the LIKE prefilter.
            if !pr_matches_commit(candidate.raw_json.as_deref().unwrap_or(""), &sha)? {
                continue;
            }
            let key = DedupKey(vec![
                KeyPart::from(&candidate.fields[0].1),
                KeyPart::from(&candidate.fields[1].1),
                KeyPart::from(&candidate.fields[2].1),
            ]);
            prs.insert(key, object(&candidate.fields));
        }

        for run in ci_runs_for(conn, &sha)? {
            let key = DedupKey(vec![
                KeyPart::from(&run.fields[0].1),
                KeyPart::from(&run.fields[2].1),
            ]);
            cis.insert(key, object(&run.fields));
        }
    }

    Ok(Outcomes {
        commits: commit_dicts,
        prs: prs.into_values(),
        ci_runs: cis.into_values(),
    })
}

/// A `pr_outcomes` row: the eight payload fields in SELECT order, plus the
/// `raw_json` the matcher reads and the payload does *not* carry.
struct PrCandidate {
    fields: Vec<(&'static str, SqlValue)>,
    raw_json: Option<String>,
}

/// `SELECT … FROM pr_outcomes WHERE raw_json LIKE ?` with `f"%{sha}%"`.
///
/// The needle is a full 40-char hex sha, so it can contain neither `%` nor `_`
/// and needs no `ESCAPE` clause — which is why Python does not have one either.
fn pr_candidates(conn: &Connection, sha: &str) -> rusqlite::Result<Vec<PrCandidate>> {
    let mut stmt = conn.prepare(
        "SELECT provider, repo_slug, pr_number, title, state, merged_at, reverted_at, author, \
         raw_json FROM pr_outcomes WHERE raw_json LIKE ?",
    )?;
    stmt.query_map([format!("%{sha}%")], |row| {
        Ok(PrCandidate {
            fields: vec![
                ("provider", row.get(0)?),
                ("repo_slug", row.get(1)?),
                ("pr_number", row.get(2)?),
                ("title", row.get(3)?),
                ("state", row.get(4)?),
                ("merged_at", row.get(5)?),
                ("reverted_at", row.get(6)?),
                ("author", row.get(7)?),
            ],
            raw_json: row.get::<_, Option<String>>(8)?,
        })
    })?
    .collect()
}

/// A `ci_runs` row: eight fields, SELECT order.
struct CiRun {
    fields: Vec<(&'static str, SqlValue)>,
}

/// `SELECT … FROM ci_runs WHERE commit_sha = ?` — an exact match, not a `LIKE`.
fn ci_runs_for(conn: &Connection, sha: &str) -> rusqlite::Result<Vec<CiRun>> {
    let mut stmt = conn.prepare(
        "SELECT provider, repo_slug, run_id, commit_sha, status, workflow_name, started_ts, \
         completed_ts FROM ci_runs WHERE commit_sha = ?",
    )?;
    stmt.query_map([sha], |row| {
        Ok(CiRun {
            fields: vec![
                ("provider", row.get(0)?),
                ("repo_slug", row.get(1)?),
                ("run_id", row.get(2)?),
                ("commit_sha", row.get(3)?),
                ("status", row.get(4)?),
                ("workflow_name", row.get(5)?),
                ("started_ts", row.get(6)?),
                ("completed_ts", row.get(7)?),
            ],
        })
    })?
    .collect()
}

/// `_pr_matches_commit(raw_json_str, commit_sha)`.
///
/// The four places a webhook payload can name a commit: GitHub's
/// `pull_request.head.sha` and `pull_request.merge_commit_sha`, and GitLab's
/// `object_attributes.last_commit.id` and `object_attributes.merge_commit_sha`.
/// Note that the GitLab pair is read off the **top-level** object, not off `pr`
/// — so a payload wrapped in `pull_request` still has its `object_attributes`
/// consulted at the root. That is what the Python does.
///
/// # Errors
/// [`OutcomeError::NonObjectPayload`] when `raw_json` decodes to a non-object;
/// see the module docs for why that is an error and not a `false`.
pub fn pr_matches_commit(raw_json: &str, commit_sha: &str) -> Result<bool, OutcomeError> {
    // `except Exception: return False` — and ONLY `json.loads` is inside it.
    let Ok(data) = serde_json::from_str::<Value>(raw_json) else {
        return Ok(false);
    };
    let Some(data) = data.as_object() else {
        return Err(OutcomeError::NonObjectPayload);
    };

    // `pr = data.get("pull_request", data)` — the whole payload when the key is
    // absent, so a bare PR object works unwrapped.
    let pr = match data.get("pull_request") {
        None => data,
        // `if not isinstance(pr, dict): return False` — a `"pull_request": null`
        // stops the whole check, GitLab branch included.
        Some(value) => match value.as_object() {
            Some(object) => object,
            None => return Ok(false),
        },
    };

    let head_sha = pr
        .get("head")
        .and_then(Value::as_object)
        .and_then(|head| head.get("sha"));
    let merge_sha = pr.get("merge_commit_sha");
    if is_sha(head_sha, commit_sha) || is_sha(merge_sha, commit_sha) {
        return Ok(true);
    }

    // `obj_attr = data.get("object_attributes", {})` — off `data`, not `pr`.
    if let Some(attrs) = data.get("object_attributes").and_then(Value::as_object) {
        let last_sha = attrs
            .get("last_commit")
            .and_then(Value::as_object)
            .and_then(|commit| commit.get("id"));
        let merge_commit_sha = attrs.get("merge_commit_sha");
        if is_sha(last_sha, commit_sha) || is_sha(merge_commit_sha, commit_sha) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// `value == commit_sha` where `value` may be any JSON — Python's `==` is only
/// ever true here for the same string.
fn is_sha(value: Option<&Value>, commit_sha: &str) -> bool {
    value.and_then(Value::as_str) == Some(commit_sha)
}

// ── the ordered dedup ────────────────────────────────────────────────────────

/// One component of a dedup key.
///
/// Python keys on a tuple of raw sqlite values, so `None`, ints and strings all
/// participate. An integral `REAL` folds into `Int` because CPython's `1 == 1.0`
/// and `hash(1) == hash(1.0)` make them the same dict key; a non-integral one
/// keeps its bits, which is close enough for a column that is declared INTEGER.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum KeyPart {
    Null,
    Int(i64),
    Bits(u64),
    Text(String),
    Blob(Vec<u8>),
}

impl From<&SqlValue> for KeyPart {
    fn from(value: &SqlValue) -> Self {
        match value {
            SqlValue::Null => Self::Null,
            SqlValue::Integer(number) => Self::Int(*number),
            #[allow(
                clippy::cast_possible_truncation,
                reason = "the guard proves the value is an exact integer"
            )]
            SqlValue::Real(number)
                if number.fract() == 0.0 && number.abs() < 9.007_199_254_740_992e15 =>
            {
                Self::Int(*number as i64)
            }
            SqlValue::Real(number) => Self::Bits(number.to_bits()),
            SqlValue::Text(text) => Self::Text(text.clone()),
            SqlValue::Blob(bytes) => Self::Blob(bytes.clone()),
        }
    }
}

/// The Python tuple used as a dict key.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct DedupKey(Vec<KeyPart>);

/// `unique[key] = value` over a CPython `dict`.
///
/// Re-assigning an existing key **updates the value and keeps the original
/// position**. That is the whole reason this type exists instead of a
/// `HashMap`: `list(unique.values())` is first-sighting order carrying
/// last-sighting fields.
#[derive(Debug, Default)]
struct OrderedDedup {
    slots: Vec<Value>,
    index: HashMap<DedupKey, usize>,
}

impl OrderedDedup {
    fn insert(&mut self, key: DedupKey, value: Value) {
        match self.index.get(&key) {
            Some(slot) => self.slots[*slot] = value,
            None => {
                self.index.insert(key, self.slots.len());
                self.slots.push(value);
            }
        }
    }

    fn into_values(self) -> Vec<Value> {
        self.slots
    }
}

// ── sqlite → JSON ────────────────────────────────────────────────────────────

/// Build a dict literal, preserving the declaration order.
fn object(fields: &[(&str, SqlValue)]) -> Value {
    let mut out = Map::new();
    for (key, value) in fields {
        out.insert((*key).to_owned(), sql_to_json(value));
    }
    Value::Object(out)
}

/// The value `json.dumps` writes for a raw `sqlite3` column.
fn sql_to_json(value: &SqlValue) -> Value {
    match value {
        SqlValue::Null => Value::Null,
        SqlValue::Integer(number) => Value::from(*number),
        SqlValue::Real(number) => stax_etl::stats::aggregator::jf(*number),
        SqlValue::Text(text) => Value::String(text.clone()),
        // `json.dumps(bytes)` raises `TypeError` — a 500 in Python. Unreachable:
        // every column selected here is declared TEXT or INTEGER. `null` is the
        // honest stand-in for "CPython would have refused to write anything".
        SqlValue::Blob(_) => Value::Null,
    }
}

/// `str(value)` for the sqlite values a sha column can hold.
fn py_str(value: &SqlValue) -> String {
    match value {
        SqlValue::Null => "None".to_owned(),
        SqlValue::Integer(number) => number.to_string(),
        SqlValue::Real(number) => {
            stax_memory::pyjson::dumps_http(&stax_etl::stats::aggregator::jf(*number))
        }
        SqlValue::Text(text) => text.clone(),
        SqlValue::Blob(_) => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The three tables `get_outcomes_for_session` reads, schema-only.
    fn store() -> Connection {
        let conn = Connection::open_in_memory().expect("in-memory");
        conn.execute_batch(
            "CREATE TABLE commit_session_link (
                 id INTEGER PRIMARY KEY, session_id TEXT, commit_sha TEXT,
                 repo_slug TEXT, committed_at TEXT);
             CREATE TABLE pr_outcomes (
                 id INTEGER PRIMARY KEY, provider TEXT, repo_slug TEXT, pr_number INTEGER,
                 title TEXT, state TEXT, merged_at TEXT, reverted_at TEXT, author TEXT,
                 raw_json TEXT);
             CREATE TABLE ci_runs (
                 id INTEGER PRIMARY KEY, provider TEXT, repo_slug TEXT, run_id TEXT,
                 commit_sha TEXT, status TEXT, workflow_name TEXT, started_ts TEXT,
                 completed_ts TEXT, raw_json TEXT);",
        )
        .expect("schema");
        conn
    }

    #[test]
    fn a_session_with_no_links_yields_three_empty_lists() {
        let conn = store();
        let outcomes = get_outcomes_for_session(&conn, "nobody").expect("query");
        assert!(outcomes.commits.is_empty());
        // The route stamps these straight onto the entry, so `[]` and not
        // `null` is the contract every yield row without a link relies on.
        assert_eq!(
            stax_memory::pyjson::dumps_http(&Value::Array(outcomes.prs)),
            "[]"
        );
        assert_eq!(
            stax_memory::pyjson::dumps_http(&Value::Array(outcomes.ci_runs)),
            "[]"
        );
    }

    #[test]
    fn a_commit_row_renders_its_three_columns_in_select_order() {
        let conn = store();
        conn.execute(
            "INSERT INTO commit_session_link (session_id, commit_sha, repo_slug, committed_at) \
             VALUES ('s1', 'abc123', 'o/r', '2026-07-01T00:00:00+00:00')",
            [],
        )
        .expect("insert");
        let outcomes = get_outcomes_for_session(&conn, "s1").expect("query");
        assert_eq!(
            stax_memory::pyjson::dumps_http(&Value::Array(outcomes.commits)),
            r#"[{"commit_sha":"abc123","repo_slug":"o/r","committed_at":"2026-07-01T00:00:00+00:00"}]"#
        );
    }

    #[test]
    fn a_pr_that_only_mentions_the_sha_in_passing_is_dropped_by_the_json_recheck() {
        let conn = store();
        let sha = "a".repeat(40);
        conn.execute(
            "INSERT INTO commit_session_link (session_id, commit_sha) VALUES ('s1', ?)",
            [&sha],
        )
        .expect("insert");
        // The LIKE finds it — the sha is in the blob — but none of the four
        // fields the matcher reads is it, so it must NOT reach the payload.
        conn.execute(
            "INSERT INTO pr_outcomes (provider, repo_slug, pr_number, raw_json) \
             VALUES ('github', 'o/r', 1, ?)",
            [&format!(r#"{{"base": {{"sha": "{sha}"}}}}"#)],
        )
        .expect("insert");
        let outcomes = get_outcomes_for_session(&conn, "s1").expect("query");
        assert!(outcomes.prs.is_empty());
    }

    #[test]
    fn a_matching_pr_ships_eight_fields_and_never_the_raw_json() {
        let conn = store();
        let sha = "b".repeat(40);
        conn.execute(
            "INSERT INTO commit_session_link (session_id, commit_sha) VALUES ('s1', ?)",
            [&sha],
        )
        .expect("insert");
        conn.execute(
            "INSERT INTO pr_outcomes \
             (provider, repo_slug, pr_number, title, state, merged_at, reverted_at, author, raw_json) \
             VALUES ('github', 'o/r', 7, 'a title', 'merged', '2026-07-02', NULL, 'yad', ?)",
            [&format!(r#"{{"pull_request": {{"head": {{"sha": "{sha}"}}}}}}"#)],
        )
        .expect("insert");
        let outcomes = get_outcomes_for_session(&conn, "s1").expect("query");
        assert_eq!(
            stax_memory::pyjson::dumps_http(&Value::Array(outcomes.prs)),
            r#"[{"provider":"github","repo_slug":"o/r","pr_number":7,"title":"a title","state":"merged","merged_at":"2026-07-02","reverted_at":null,"author":"yad"}]"#
        );
    }

    #[test]
    fn a_pr_seen_twice_keeps_the_first_position_and_the_last_value() {
        let conn = store();
        // Two commits, both linked to the session; one PR matches both, and the
        // second sighting must overwrite the row IN PLACE — behind an unrelated
        // PR that was seen first.
        let sha_a = "c".repeat(40);
        let sha_b = "d".repeat(40);
        conn.execute(
            "INSERT INTO commit_session_link (session_id, commit_sha) VALUES ('s1', ?)",
            [&sha_a],
        )
        .expect("insert");
        conn.execute(
            "INSERT INTO commit_session_link (session_id, commit_sha) VALUES ('s1', ?)",
            [&sha_b],
        )
        .expect("insert");
        conn.execute(
            "INSERT INTO pr_outcomes (provider, repo_slug, pr_number, title, raw_json) \
             VALUES ('github', 'o/r', 1, 'first sighting', ?)",
            [&format!(r#"{{"head": {{"sha": "{sha_a}"}}}}"#)],
        )
        .expect("insert");
        // Same (provider, repo_slug, pr_number) key, different title, matched
        // via the SECOND commit.
        conn.execute(
            "INSERT INTO pr_outcomes (provider, repo_slug, pr_number, title, raw_json) \
             VALUES ('github', 'o/r', 1, 'second sighting', ?)",
            [&format!(r#"{{"merge_commit_sha": "{sha_b}"}}"#)],
        )
        .expect("insert");
        conn.execute(
            "INSERT INTO pr_outcomes (provider, repo_slug, pr_number, title, raw_json) \
             VALUES ('github', 'o/r', 2, 'other pr', ?)",
            [&format!(r#"{{"head": {{"sha": "{sha_b}"}}}}"#)],
        )
        .expect("insert");

        let outcomes = get_outcomes_for_session(&conn, "s1").expect("query");
        let titles: Vec<&str> = outcomes
            .prs
            .iter()
            .filter_map(|pr| pr.get("title").and_then(Value::as_str))
            .collect();
        // PR 1 is still FIRST (it was sighted first) but now carries the SECOND
        // row's title. A HashMap loses the order; a remove-then-insert moves
        // PR 1 behind PR 2.
        assert_eq!(titles, vec!["second sighting", "other pr"]);
    }

    #[test]
    fn ci_runs_dedup_on_provider_and_run_id_only_not_the_whole_row() {
        let conn = store();
        let sha = "e".repeat(40);
        conn.execute(
            "INSERT INTO commit_session_link (session_id, commit_sha) VALUES ('s1', ?)",
            [&sha],
        )
        .expect("insert");
        for status in ["queued", "success"] {
            conn.execute(
                "INSERT INTO ci_runs (provider, repo_slug, run_id, commit_sha, status) \
                 VALUES ('github', 'o/r', 'run-1', ?, ?)",
                rusqlite::params![&sha, status],
            )
            .expect("insert");
        }
        let outcomes = get_outcomes_for_session(&conn, "s1").expect("query");
        assert_eq!(outcomes.ci_runs.len(), 1);
        // Last write wins on the value.
        assert_eq!(
            outcomes.ci_runs[0].get("status").and_then(Value::as_str),
            Some("success")
        );
    }

    #[test]
    fn the_matcher_reads_all_four_webhook_shapes() {
        let sha = "abc";
        for payload in [
            r#"{"pull_request": {"head": {"sha": "abc"}}}"#,
            r#"{"pull_request": {"merge_commit_sha": "abc"}}"#,
            r#"{"head": {"sha": "abc"}}"#,
            r#"{"object_attributes": {"last_commit": {"id": "abc"}}}"#,
            r#"{"object_attributes": {"merge_commit_sha": "abc"}}"#,
        ] {
            assert!(
                pr_matches_commit(payload, sha).expect("object"),
                "{payload}"
            );
        }
        for payload in [
            r#"{"head": {"sha": "different"}}"#,
            r#"{"head": "not an object"}"#,
            r#"{"object_attributes": {"last_commit": "not an object"}}"#,
            r#"{}"#,
        ] {
            assert!(
                !pr_matches_commit(payload, sha).expect("object"),
                "{payload}"
            );
        }
    }

    #[test]
    fn a_null_pull_request_key_stops_the_check_before_the_gitlab_branch() {
        // `pr = data.get("pull_request", data)` finds `None`, and
        // `isinstance(None, dict)` is False — so the function returns early and
        // never looks at `object_attributes`, even though it would have matched.
        let payload = r#"{"pull_request": null, "object_attributes": {"merge_commit_sha": "abc"}}"#;
        assert!(!pr_matches_commit(payload, "abc").expect("object"));
    }

    #[test]
    fn unparseable_json_is_false_but_a_json_list_is_an_error() {
        assert!(!pr_matches_commit("not json at all", "abc").expect("caught"));
        // `except Exception` wraps `json.loads` ONLY; `data.get` on a list is an
        // uncaught `AttributeError`, i.e. a 500.
        assert!(matches!(
            pr_matches_commit(r#"[1, 2]"#, "abc"),
            Err(OutcomeError::NonObjectPayload)
        ));
        assert!(matches!(
            pr_matches_commit("42", "abc"),
            Err(OutcomeError::NonObjectPayload)
        ));
    }

    #[test]
    fn integral_reals_and_ints_are_the_same_dedup_key_as_they_are_in_python() {
        assert_eq!(
            KeyPart::from(&SqlValue::Integer(1)),
            KeyPart::from(&SqlValue::Real(1.0))
        );
        assert_ne!(
            KeyPart::from(&SqlValue::Integer(1)),
            KeyPart::from(&SqlValue::Real(1.5))
        );
        assert_ne!(
            KeyPart::from(&SqlValue::Null),
            KeyPart::from(&SqlValue::Integer(0))
        );
    }
}
