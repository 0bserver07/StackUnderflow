//! `SearchService.lexical_session_hits` — the retriever the `memory` verbs use
//! when `search_index.db` is populated.
//!
//! Four of the five `memory` verbs take a **different code path** depending on
//! this file's contents. `cli._lexical_search_service()` hands
//! `services/discovery.py` a `SearchService` on every `memory decisions`,
//! `memory worked`, `memory file` and `memory sessions <file>` invocation; when
//! the index has rows, the leading-wildcard `content_text LIKE '%needle%'`
//! full scan is replaced by an FTS5 `MATCH` + bm25 `rank` lookup that is then
//! *clustered* to one representative message per session. On the maintainer's
//! machine the index holds 250,998 messages, so the bm25 branch is not a future
//! feature — it is the branch that runs today, and the LIKE branch is the
//! fallback.
//!
//! The two databases are joined **in Rust, by `session_id`**, never by
//! `ATTACH`: they have independent WAL and lock domains, and `memory ask`
//! already bridges them the same way ([`crate::ask`]).
//!
//! Three signals come out of [`LexicalIndex::session_hits`] and each one means
//! something different downstream — getting them confused silently changes
//! answers:
//!
//! * `None` — the index is absent, unreadable, or has **no rows**. The only
//!   "not populated" signal, and the only one that makes the caller fall back
//!   to the LIKE scan.
//! * `Some(vec![])` — the index is populated and genuinely matched nothing.
//!   The caller must *not* fall back; reintroducing the full scan on a real
//!   no-match is the anti-pattern this path exists to remove.
//! * `Some(hits)` — best-bm25-first, one row per session.
//!
//! Read-only by construction, which is one recorded divergence: Python's
//! `SearchService.__init__` **creates** `search_index.db` and applies its
//! schema as a side effect of merely asking a question. This port opens what is
//! there and never writes (wave-0 decision; the empty database Python leaves
//! behind reads as unpopulated on both sides, so no output differs).

use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags};

use crate::ask::{build_filter_clauses, sanitize_fts_query};

/// One clustered lexical hit — `lexical_session_hits`' dict, typed.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionHit {
    /// `messages.session_id` from the index; the join key into the store.
    pub session_id: String,
    /// The representative (best-ranked) message's full text.
    ///
    /// Not an FTS `snippet()`: the caller builds the same Python snippet the
    /// LIKE path builds, so the snippet format is identical across both paths.
    pub content: String,
    /// SQLite's raw bm25 `rank` — **negative, lower is better**.
    pub bm25: f64,
    /// Further matching messages this session had, beyond the representative.
    pub more_matches_in_session: i64,
}

/// A handle on `search_index.db`, opened per query as Python opens it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LexicalIndex {
    path: PathBuf,
}

impl LexicalIndex {
    /// The index beside a store — `Path(deps.store_path).parent /
    /// "search_index.db"`, the derivation `cli._lexical_search_service` uses.
    #[must_use]
    pub fn beside_store(store_path: &Path) -> Option<Self> {
        Some(Self {
            path: store_path.parent()?.join("search_index.db"),
        })
    }

    /// An index at an explicit path — what tests and the parity harness use.
    #[must_use]
    pub fn at(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// Where this index lives.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// `SearchService.lexical_session_hits` — best-bm25-first, one row per
    /// session, or `None` when the index is not populated.
    ///
    /// Never fails: every operational error degrades exactly as Python's does
    /// — a missing/unreadable database or an empty `messages` table is `None`
    /// (fall back to LIKE), and a `MATCH` that the engine rejects after
    /// sanitising is `Some(vec![])` (a genuine no-match).
    #[must_use]
    pub fn session_hits(
        &self,
        query: &str,
        project: Option<&str>,
        date_from: Option<&str>,
        candidate_k: i64,
    ) -> Option<Vec<SessionHit>> {
        // `if not query or not query.strip(): return []` — note this is the
        // populated-looking answer, before the index is even opened.
        if query.trim().is_empty() {
            return Some(Vec::new());
        }
        let conn = open_read_only(&self.path)?;

        // The populated probe. Python catches `OperationalError` (no `messages`
        // table) and a `None` fetch (no rows) into the same `return None`.
        let populated = conn
            .query_row("SELECT 1 FROM messages LIMIT 1", [], |row| {
                row.get::<_, i64>(0)
            })
            .ok();
        populated?;

        let safe_query = sanitize_fts_query(query);
        let (where_sql, params) = build_filter_clauses(project, date_from, None, None, None);
        let sql = format!(
            "SELECT m.session_id AS session_id, m.content AS content, rank AS bm25 \
             FROM messages_fts \
             JOIN messages m ON messages_fts.rowid = m.id \
             WHERE messages_fts MATCH ? \
             {where_sql} \
             ORDER BY rank \
             LIMIT ?"
        );
        let mut bound: Vec<rusqlite::types::Value> = vec![rusqlite::types::Value::Text(safe_query)];
        bound.extend(
            params
                .iter()
                .map(|param| rusqlite::types::Value::Text(param.clone())),
        );
        // `max(1, int(candidate_k))`.
        bound.push(rusqlite::types::Value::Integer(candidate_k.max(1)));

        // An FTS5 syntax hiccup that survived sanitising is `[]`, never `None`.
        let Ok(mut stmt) = conn.prepare(&sql) else {
            return Some(Vec::new());
        };
        let Ok(rows) = stmt
            .query_map(rusqlite::params_from_iter(bound.iter()), |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?.unwrap_or_default(),
                    row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                    row.get::<_, f64>(2)?,
                ))
            })
            .and_then(Iterator::collect::<rusqlite::Result<Vec<_>>>)
        else {
            return Some(Vec::new());
        };

        // Python builds a dict keyed by session_id; insertion order is
        // `ORDER BY rank`, i.e. best first, and a repeat hit only bumps the
        // count. A `Vec` plus a position lookup reproduces that exactly
        // (candidate_k is 200-500, so the linear probe is not the cost).
        let mut best: Vec<SessionHit> = Vec::new();
        for (session_id, content, bm25) in rows {
            if session_id.is_empty() {
                continue;
            }
            if let Some(existing) = best.iter_mut().find(|hit| hit.session_id == session_id) {
                existing.more_matches_in_session += 1;
                continue;
            }
            best.push(SessionHit {
                session_id,
                content,
                bm25,
                more_matches_in_session: 0,
            });
        }
        Some(best)
    }
}

/// `candidate_k = max(int(limit) * 10, 200) if limit and limit > 0 else 500`.
///
/// `None` when the whole lexical call is doomed — and that case is not
/// theoretical, it is a measured divergence. Python's int is unbounded, so
/// `--limit 99999999999999999999` computes `candidate_k = 999999999999999999990`
/// happily and only dies at the `LIMIT ?` bind, with SQLite's
/// `OverflowError: Python int too large to convert to SQLite INTEGER`. That is
/// **not** an `OperationalError`, so `lexical_session_hits`' inner handler does
/// not catch it; it escapes to the `except Exception` around the call in
/// `_fts_decisions` / `_touching_content_half` / `_action_worked_fts`, every one
/// of which answers `None` — "no lexical half" — and the query falls all the way
/// back to the `LIKE` scan.
///
/// So a big-enough `--limit` silently *changes which engine answers*. Returning
/// `None` here reproduces that: the callers treat it exactly as they treat an
/// unpopulated index. Saturating instead left the port on the bm25 path where
/// Python was on the LIKE path — same exit code, different answer, which the
/// byte-diff harness caught on the populated-FTS state.
#[must_use]
pub fn candidate_k(limit: i64) -> Option<i64> {
    if limit <= 0 {
        return Some(500);
    }
    // The bind fails iff `limit * 10` exceeds a signed 64-bit integer; note the
    // `max(…, 200)` cannot rescue it, because Python computes the product first.
    if limit > i64::MAX / 10 {
        return None;
    }
    Some((limit * 10).max(200))
}

/// `_bm25_relevance` — min-max the raw bm25 `rank` into `[0, 1]`, best = 1.0.
///
/// FTS5's `rank` is negative and lower is better, which is the opposite shape
/// the packer's rank function wants (each term in `[0, 1]`, higher better).
/// All-equal candidates all score `1.0`, as does a single candidate.
#[must_use]
pub fn bm25_relevance(hits: &[(String, f64)]) -> Vec<(String, f64)> {
    if hits.is_empty() {
        return Vec::new();
    }
    let mut low = f64::INFINITY;
    let mut high = f64::NEG_INFINITY;
    for (_, value) in hits {
        low = low.min(*value);
        high = high.max(*value);
    }
    let span = high - low;
    if span <= 0.0 {
        return hits
            .iter()
            .map(|(session_id, _)| (session_id.clone(), 1.0))
            .collect();
    }
    hits.iter()
        .map(|(session_id, value)| (session_id.clone(), 1.0 - (value - low) / span))
        .collect()
}

/// Open a sidecar strictly read-only; `None` when it is not there.
fn open_read_only(path: &Path) -> Option<Connection> {
    if !path.exists() {
        return None;
    }
    Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .ok()
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    /// A scratch directory that cleans up after itself.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "stax-lexical-{tag}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0)
            ));
            std::fs::create_dir_all(&dir).expect("scratch dir");
            Self(dir)
        }

        fn path(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A `search_index.db` with the reference schema and `rows` indexed.
    fn build_index(path: &std::path::Path, rows: &[(&str, &str, &str, &str)]) {
        let conn = Connection::open(path).expect("create index");
        conn.execute_batch(
            "CREATE TABLE messages (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 session_id TEXT NOT NULL,
                 project TEXT NOT NULL,
                 role TEXT NOT NULL,
                 content TEXT NOT NULL,
                 timestamp TEXT,
                 model TEXT,
                 tokens_input INTEGER DEFAULT 0,
                 tokens_output INTEGER DEFAULT 0
             );
             CREATE VIRTUAL TABLE messages_fts USING fts5(
                 content, content='messages', content_rowid='id',
                 tokenize='porter unicode61'
             );
             CREATE TRIGGER messages_ai AFTER INSERT ON messages BEGIN
                 INSERT INTO messages_fts(rowid, content) VALUES (new.id, new.content);
             END;",
        )
        .expect("schema");
        for (session_id, project, content, timestamp) in rows {
            conn.execute(
                "INSERT INTO messages (session_id, project, role, content, timestamp) \
                 VALUES (?, ?, 'assistant', ?, ?)",
                rusqlite::params![session_id, project, content, timestamp],
            )
            .expect("insert");
        }
    }

    /// The same schema with no rows — Python's "fresh install" state.
    fn build_empty_index(path: &std::path::Path) {
        build_index(path, &[]);
    }

    #[test]
    fn a_missing_index_is_unpopulated() {
        let index = LexicalIndex::at("/nonexistent/search_index.db");
        assert_eq!(index.session_hits("cache", None, None, 200), None);
    }

    #[test]
    fn an_empty_index_is_unpopulated_not_an_empty_result() {
        // The distinction is the whole contract: `None` falls back to the LIKE
        // scan, `Some([])` does not.
        let scratch = Scratch::new("empty");
        let path = scratch.path("search_index.db");
        build_empty_index(&path);
        assert_eq!(
            LexicalIndex::at(&path).session_hits("cache", None, None, 200),
            None
        );
    }

    #[test]
    fn a_populated_index_that_matches_nothing_is_an_empty_result() {
        let scratch = Scratch::new("nomatch");
        let path = scratch.path("search_index.db");
        build_index(
            &path,
            &[(
                "s1",
                "-proj",
                "totally unrelated prose",
                "2026-01-01T00:00:00",
            )],
        );
        assert_eq!(
            LexicalIndex::at(&path).session_hits("cache", None, None, 200),
            Some(Vec::new())
        );
    }

    #[test]
    fn hits_come_back_best_first_clustered_per_session() {
        let scratch = Scratch::new("cluster");
        let path = scratch.path("search_index.db");
        build_index(
            &path,
            &[
                (
                    "s1",
                    "-proj",
                    "cache cache cache everywhere",
                    "2026-01-01T00:00:00",
                ),
                (
                    "s1",
                    "-proj",
                    "a second cache mention",
                    "2026-01-02T00:00:00",
                ),
                (
                    "s2",
                    "-proj",
                    "one cache mention only",
                    "2026-01-03T00:00:00",
                ),
            ],
        );
        let hits = LexicalIndex::at(&path)
            .session_hits("cache", None, None, 200)
            .expect("populated");
        assert_eq!(hits.len(), 2, "one row per session");
        assert_eq!(hits[0].session_id, "s1", "densest session ranks first");
        assert_eq!(hits[0].more_matches_in_session, 1);
        assert_eq!(hits[1].more_matches_in_session, 0);
        assert!(hits[0].bm25 < 0.0, "SQLite's rank is negative");
        assert!(hits[0].bm25 <= hits[1].bm25, "lower rank is better");
    }

    #[test]
    fn a_phrase_that_zeroes_on_like_still_matches_here() {
        // Findings ledger #3: `LIKE '%cache lookup%'` needs the words adjacent.
        let scratch = Scratch::new("phrase");
        let path = scratch.path("search_index.db");
        build_index(
            &path,
            &[(
                "s1",
                "-proj",
                "the cache is consulted on every lookup",
                "2026-01-01T00:00:00",
            )],
        );
        let hits = LexicalIndex::at(&path)
            .session_hits("cache lookup", None, None, 200)
            .expect("populated");
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn the_project_and_date_filters_reach_the_sql() {
        let scratch = Scratch::new("filters");
        let path = scratch.path("search_index.db");
        build_index(
            &path,
            &[
                ("s1", "-alpha", "cache notes", "2026-01-01T00:00:00"),
                ("s2", "-beta", "cache notes", "2026-06-01T00:00:00"),
            ],
        );
        let index = LexicalIndex::at(&path);
        let scoped = index
            .session_hits("cache", Some("-beta"), None, 200)
            .expect("populated");
        assert_eq!(scoped.len(), 1);
        assert_eq!(scoped[0].session_id, "s2");

        let dated = index
            .session_hits("cache", None, Some("2026-03-01T00:00:00"), 200)
            .expect("populated");
        assert_eq!(dated.len(), 1);
        assert_eq!(dated[0].session_id, "s2");
    }

    #[test]
    fn an_empty_query_is_an_empty_result_without_touching_the_index() {
        // Python returns `[]` before it opens a connection, so even a missing
        // index answers "populated, nothing matched".
        let index = LexicalIndex::at("/nonexistent/search_index.db");
        assert_eq!(index.session_hits("   ", None, None, 200), Some(Vec::new()));
    }

    #[test]
    fn candidate_k_follows_the_reference_formula() {
        assert_eq!(candidate_k(20), Some(200), "20 * 10 = 200, the floor");
        assert_eq!(candidate_k(50), Some(500));
        assert_eq!(candidate_k(1), Some(200), "the floor wins under 20");
        assert_eq!(candidate_k(0), Some(500), "no limit => 500");
        assert_eq!(candidate_k(-1), Some(500));
    }

    #[test]
    fn a_limit_whose_candidate_cap_overflows_disables_the_lexical_half() {
        // The measured case: `--limit 99999999999999999999` makes Python's
        // `LIMIT ?` bind raise, the caller swallows it, and the whole query
        // reverts to the LIKE scan. `None` is how that reaches the callers.
        assert_eq!(candidate_k(i64::MAX), None);
        assert_eq!(candidate_k(i64::MAX / 10 + 1), None);
        assert_eq!(
            candidate_k(i64::MAX / 10),
            Some((i64::MAX / 10) * 10),
            "the largest limit whose product still binds"
        );
    }

    #[test]
    fn bm25_relevance_maps_best_to_one_and_worst_to_zero() {
        let scored = bm25_relevance(&[
            ("a".to_owned(), -5.0),
            ("b".to_owned(), -1.0),
            ("c".to_owned(), -3.0),
        ]);
        assert_eq!(scored[0].1, 1.0, "most negative rank is best");
        assert_eq!(scored[1].1, 0.0);
        assert!((scored[2].1 - 0.5).abs() < 1e-12);

        let flat = bm25_relevance(&[("a".to_owned(), -2.0), ("b".to_owned(), -2.0)]);
        assert_eq!(flat[0].1, 1.0);
        assert_eq!(flat[1].1, 1.0);
        assert!(bm25_relevance(&[]).is_empty());
    }
}
