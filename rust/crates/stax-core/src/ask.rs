//! `memory ask` — hybrid retrieval, ported from `cli._run_ask_query`.
//!
//! A port of the engine behind `stackunderflow memory ask` (`cli.py:1993`) and
//! everything it reaches: `_hybrid_session_order`, `_hydrate_sessions`,
//! `_fuse_session_ids`, `services/search_service.py`'s `hybrid_search` +
//! `_fts_ranked_ids` + `_rows_for_ids` + `_sanitize_fts_query` +
//! `_build_filter_clauses` + `_row_matches_filters` + `_vector_ranked_ids`, and
//! `services/embeddings.py`'s `rrf_merge` / `cosine` / `EmbeddingStore` /
//! `ollama_reachable` / `embed_texts`.
//!
//! **The one thing to know about `ask`:** its base retrieval is *always* the
//! `LIKE '%needle%'` scan. `_run_ask_query` calls `search_past_decisions`
//! without a `search_service`, so the bm25 index never widens or replaces the
//! base — unlike `memory decisions`, which does inject one. The FTS5 index and
//! the Ollama vector store only contribute a **second ordering** of session ids,
//! reciprocal-rank-fused with the base order, plus any semantic-only sessions
//! hydrated back out of the store. Both halves degrade to nothing
//! independently, and when both are absent the fused order equals the base
//! order exactly — the "keyword fallback" the CLI's note advertises.
//!
//! Three fallbacks, all silent, all ported:
//!
//! * **No `search_index.db`, or an unpopulated one** → `_fts_ranked_ids` is
//!   empty. (Python's `SearchService.__init__` *creates* the file and its
//!   schema; this port opens read-only and treats a missing index as empty —
//!   the wave-0 no-silent-create decision, recorded as a divergence.)
//! * **No vectors for the embed model in `embeddings.db`** → the vector half
//!   returns before it ever probes Ollama.
//! * **Ollama unreachable** → the vector half returns `[]` and `vector_used`
//!   stays false, which is the branch every machine without a running daemon
//!   takes and the one the reference's own tests pin by pointing every endpoint
//!   at a dead port.
//!
//! Everything here takes its configuration as an argument ([`HybridEnv`]);
//! nothing reads the process environment (wave-1 pattern law, finding 5).

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Result;
use rusqlite::{Connection, OpenFlags};

use crate::queries::{
    self, BudgetedResult, SESSION_FROM, SESSION_SELECT, SessionMatch, build_snippet, placeholders,
    pyjson, pytime, rank, row_to_match,
};

// ── constants, ported ────────────────────────────────────────────────────────

/// `embeddings.DEFAULT_EMBED_MODEL`.
pub const DEFAULT_EMBED_MODEL: &str = "nomic-embed-text";

/// `embeddings.DEFAULT_OLLAMA_URL` — also `embeddings.LOCAL_OLLAMA_URL`.
pub const DEFAULT_OLLAMA_URL: &str = "http://localhost:11434";

/// `embeddings._REACHABLE_TIMEOUT_S`.
const REACHABLE_TIMEOUT: Duration = Duration::from_millis(1_500);

/// `embeddings._EMBED_TIMEOUT_S`.
const EMBED_TIMEOUT: Duration = Duration::from_secs(30);

/// `SearchService.hybrid_search(candidate_k=…)`'s default.
const CANDIDATE_K: i64 = 50;

/// `embeddings.rrf_merge(k=…)` — the value from the original RRF paper.
const RRF_K: i64 = 60;

// ── the injected environment ─────────────────────────────────────────────────

/// Everything the hybrid half needs from the outside world, injected.
///
/// Python reads five environment variables and two module-level path constants
/// scattered across `cli`, `search_service` and `embeddings`; they are gathered
/// here so the whole engine stays a pure function of its inputs. `stax-cli`
/// resolves this once at the process edge.
#[derive(Debug, Clone)]
pub struct HybridEnv {
    /// `Path(deps.store_path).parent / "search_index.db"`, or `None` when the
    /// store path is unknown (Python then hands `SearchService` `db_path=None`,
    /// which falls back to its module default; both end up reading the same
    /// file in every real configuration).
    pub index_path: Option<PathBuf>,
    /// `embeddings.EMBEDDINGS_DB_PATH` — `app_dir()/embeddings.db`.
    pub embeddings_path: PathBuf,
    /// `embeddings._resolve_model(None)`.
    pub embed_model: String,
    /// `embeddings._resolve_url(None)` — what the **reachability probe** hits.
    ///
    /// Deliberately not the same as [`embed_endpoint`]: `_vector_ranked_ids`
    /// probes `_resolve_url(None)` (`OLLAMA_URL` or local) but embeds through
    /// `_resolve_endpoints()[0]` (`STACKUNDERFLOW_OLLAMA_URL` first). With only
    /// `STACKUNDERFLOW_OLLAMA_URL` set, Python probes localhost and then embeds
    /// against the cloud. Ported as found.
    ///
    /// [`embed_endpoint`]: HybridEnv::embed_endpoint
    pub probe_url: String,
    /// `embeddings._resolve_endpoints()[0]` — `(base, api_key)`, cloud first.
    pub embed_endpoint: Option<(String, Option<String>)>,
    /// `embeddings._resolve_api_key()` — the probe's bearer token.
    pub api_key: Option<String>,
}

impl HybridEnv {
    /// Resolve from already-read environment values — the pure constructor.
    ///
    /// `store_path` is `deps.store_path`; `app_dir` is `settings.app_dir()`.
    /// The four `*_env` arguments are the raw values of
    /// `STACKUNDERFLOW_EMBED_MODEL`, `OLLAMA_URL`, `STACKUNDERFLOW_OLLAMA_URL`
    /// and (`STACKUNDERFLOW_OLLAMA_API_KEY` else `OLLAMA_API_KEY`). An empty
    /// string counts as unset, matching `os.environ.get(...) or default`.
    #[must_use]
    pub fn resolve(
        app_dir: &Path,
        store_path: Option<&Path>,
        embed_model_env: Option<&str>,
        ollama_url_env: Option<&str>,
        cloud_url_env: Option<&str>,
        api_key_env: Option<&str>,
    ) -> Self {
        let api_key = truthy(api_key_env).map(ToOwned::to_owned);
        // `_resolve_url(None)`: OLLAMA_URL, else the default. No rstrip here —
        // Python's `_resolve_url` does not strip, and the probe URL is used raw.
        let probe_url =
            truthy(ollama_url_env).map_or_else(|| DEFAULT_OLLAMA_URL.to_owned(), ToOwned::to_owned);
        let endpoints = resolve_endpoints(ollama_url_env, cloud_url_env, api_key.as_deref());
        Self {
            index_path: store_path
                .and_then(Path::parent)
                .map(|parent| parent.join("search_index.db")),
            embeddings_path: app_dir.join("embeddings.db"),
            embed_model: truthy(embed_model_env)
                .map_or_else(|| DEFAULT_EMBED_MODEL.to_owned(), ToOwned::to_owned),
            probe_url,
            embed_endpoint: endpoints.into_iter().next(),
            api_key,
        }
    }

    /// A configuration whose vector half can never fire — no index, no vectors,
    /// and a probe URL that resolves nowhere. The shape tests use.
    #[must_use]
    pub fn disabled() -> Self {
        Self {
            index_path: None,
            embeddings_path: PathBuf::from("/nonexistent/embeddings.db"),
            embed_model: DEFAULT_EMBED_MODEL.to_owned(),
            // The reference's own hermetic fix: point every endpoint at a dead
            // port so a developer's running daemon cannot decide the outcome.
            probe_url: "http://127.0.0.1:1".to_owned(),
            embed_endpoint: Some(("http://127.0.0.1:1".to_owned(), None)),
            api_key: None,
        }
    }
}

/// `os.environ.get(NAME) or <default>` — an empty string counts as unset.
fn truthy(raw: Option<&str>) -> Option<&str> {
    raw.filter(|value| !value.is_empty())
}

/// `embeddings._resolve_endpoints(None)` — cloud first (rstrip'd), then local
/// unless the cloud entry already *is* local.
///
/// [`HybridEnv`] keeps only the first entry because `_vector_ranked_ids` embeds
/// through `_resolve_endpoints()[0]`; `embed_texts` instead walks the whole list
/// and takes the first one that answers, so the full list is what it needs.
#[must_use]
pub fn resolve_endpoints(
    ollama_url_env: Option<&str>,
    cloud_url_env: Option<&str>,
    api_key: Option<&str>,
) -> Vec<(String, Option<String>)> {
    let cloud = truthy(cloud_url_env).or_else(|| truthy(ollama_url_env));
    let mut endpoints: Vec<(String, Option<String>)> = Vec::new();
    if let Some(cloud) = cloud {
        endpoints.push((
            cloud.trim_end_matches('/').to_owned(),
            api_key.map(ToOwned::to_owned),
        ));
    }
    if endpoints.iter().all(|(base, _)| base != DEFAULT_OLLAMA_URL) {
        endpoints.push((DEFAULT_OLLAMA_URL.to_owned(), None));
    }
    endpoints
}

/// `embeddings.active_endpoint()` — the first endpoint whose `/api/tags` says 200.
#[must_use]
pub fn active_endpoint(
    endpoints: &[(String, Option<String>)],
) -> Option<&(String, Option<String>)> {
    endpoints
        .iter()
        .find(|(base, key)| ollama_reachable(base, key.as_deref()))
}

/// `embeddings.embed_texts(texts, model=…)` — one vector per input that worked.
///
/// Three properties are the contract, and each one is load-bearing for the
/// `search-past-decisions --use-embeddings` caller:
///
/// * `[]` in ⇒ `Some(vec![])` out — never a probe.
/// * A row that fails to embed is **absent** from the result rather than
///   zero-filled, so the caller can tell partial failure from total: it length-
///   checks the batch and discards a short answer.
/// * Nothing reachable, or every row failed ⇒ `None`, which the caller reads as
///   "embeddings unavailable" and degrades to substring ranking, silently.
#[must_use]
pub fn embed_texts(
    texts: &[String],
    model: &str,
    endpoints: &[(String, Option<String>)],
) -> Option<Vec<Vec<f64>>> {
    if texts.is_empty() {
        return Some(Vec::new());
    }
    let (base, api_key) = active_endpoint(endpoints)?;
    let out: Vec<Vec<f64>> = texts
        .iter()
        .filter_map(|text| embed_one(base, model, text, api_key.as_deref()))
        .collect();
    if out.is_empty() { None } else { Some(out) }
}

// ── what `ask` produces ──────────────────────────────────────────────────────

/// `cli._run_ask_query`'s `(BudgetedResult, resolved_slug, vector_used)`.
#[derive(Debug, Clone, PartialEq)]
pub struct AskOutcome {
    /// The packed, fused, provenance-carrying rows.
    pub result: BudgetedResult,
    /// The project slug the query was actually scoped to.
    pub slug: Option<String>,
    /// Whether the semantic half contributed anything.
    pub vector_used: bool,
}

/// One `memory ask` invocation's inputs — `_run_ask_query`'s keyword arguments.
#[derive(Debug, Clone, Copy)]
pub struct AskRequest<'a> {
    /// The natural-language question.
    pub question: &'a str,
    /// `--project`; `None` falls back to `cwd` when `scope_to_cwd` is set.
    pub project: Option<&'a str>,
    /// `--since`.
    pub since: Option<&'a str>,
    /// `--limit`.
    pub limit: i64,
    /// The `memory` namespace's default; the back-compat aliases leave it off.
    pub scope_to_cwd: bool,
    /// `Path.cwd()` as a string.
    pub cwd: &'a str,
}

/// `cli._run_ask_query` — the whole hybrid pipeline on one store connection.
///
/// # Errors
/// When the base query fails, or `since` is malformed (`ValueError` in Python).
/// The hybrid half never fails the call — every miss degrades to no contribution.
pub fn run_ask_query(
    conn: &Connection,
    request: &AskRequest<'_>,
    budget: &rank::Budget,
    env: &HybridEnv,
) -> Result<AskOutcome> {
    let AskRequest {
        question,
        project,
        since,
        limit,
        scope_to_cwd,
        cwd,
    } = *request;
    let slug = match project {
        Some(slug) => Some(slug.to_owned()),
        None if scope_to_cwd => queries::detect_cwd_project_slug(conn, cwd),
        None => None,
    };

    // Base (provenance authority) — no budget yet; we pack after fusion.
    let base = search_past_decisions_unbudgeted(conn, question, slug.as_deref(), since, limit)?;
    // Insertion-ordered like CPython's dict: `by_sid` is keyed lookup, `order`
    // is the iteration order the fusion needs.
    let mut by_sid: HashMap<String, SessionMatch> = HashMap::with_capacity(base.len());
    let mut base_order: Vec<String> = Vec::with_capacity(base.len());
    for session_match in base {
        base_order.push(session_match.session_id.clone());
        by_sid.insert(session_match.session_id.clone(), session_match);
    }

    // Hybrid semantic ordering of session ids (best-effort; empty on any miss).
    let (hybrid_sids, vector_used) = hybrid_session_order(env, question, slug.as_deref(), limit);

    // Pull in semantic-only sessions the substring base missed, looking up
    // provenance from the store. Best-effort: a session we can't hydrate is
    // simply skipped (it stays out of the fused surface).
    let extra_sids: Vec<String> = hybrid_sids
        .iter()
        .filter(|sid| !by_sid.contains_key(*sid))
        .cloned()
        .collect();
    if !extra_sids.is_empty() {
        for session_match in hydrate_sessions(conn, &extra_sids, slug.as_deref()) {
            by_sid
                .entry(session_match.session_id.clone())
                .or_insert(session_match);
        }
    }

    // Fuse the two session orderings with RRF. When `hybrid_sids` is empty this
    // returns `base_order` unchanged — the no-Ollama / empty-index path is a
    // no-op re-rank (zero regression vs the keyword-only surface).
    let ordered_sids = fuse_session_ids(&base_order, &hybrid_sids, RRF_K);

    let mut ordered: Vec<SessionMatch> = ordered_sids
        .iter()
        .filter_map(|sid| by_sid.get(sid).cloned())
        .collect();
    if limit > 0 {
        ordered.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
    }

    // `rank_fn=None`: the fused order IS the ranking; the budget only trims it.
    let (kept, dropped, used) = rank::pack_within_budget(ordered, budget.tokens, None);
    Ok(AskOutcome {
        result: BudgetedResult {
            sessions: kept,
            truncated: dropped > 0,
            more_available: dropped,
            budget_used_tokens: used,
            budget_max_tokens: budget.tokens,
        },
        slug,
        vector_used,
    })
}

/// `discovery.search_past_decisions(context_budget=None, search_service=None)`.
///
/// The plain-list overload: `last_ts DESC`, hard-capped at `limit`, with
/// Python-built snippets and the session-clustering count. Deliberately *not*
/// [`queries::search_past_decisions`], which always applies the budget and
/// therefore re-ranks — `ask` must fuse the recency order, not the ranked one.
///
/// # Errors
/// When a query fails, or `since` is malformed.
pub fn search_past_decisions_unbudgeted(
    conn: &Connection,
    query: &str,
    project: Option<&str>,
    since: Option<&str>,
    limit: i64,
) -> Result<Vec<SessionMatch>> {
    if query.trim().is_empty() {
        return Ok(Vec::new());
    }
    let needle = query.trim();
    let since_iso = pytime::parse_since(since)?;

    let mut sql = String::from(
        "SELECT m.id AS mid, m.session_fk AS sfk, s.session_id AS sid, \
         m.content_text AS content_text \
         FROM messages m \
         JOIN sessions s ON s.id = m.session_fk \
         JOIN projects p ON p.id = s.project_id \
         WHERE m.content_text LIKE ?",
    );
    let mut params: Vec<rusqlite::types::Value> =
        vec![rusqlite::types::Value::Text(format!("%{needle}%"))];
    if let Some(slug) = project {
        sql.push_str(" AND p.slug = ?");
        params.push(rusqlite::types::Value::Text(slug.to_owned()));
    }
    if let Some(iso) = &since_iso {
        sql.push_str(" AND m.timestamp >= ?");
        params.push(rusqlite::types::Value::Text(iso.clone()));
    }
    sql.push_str(" ORDER BY m.timestamp DESC");

    let mut stmt = conn.prepare(&sql)?;
    let hit_rows = stmt
        .query_map(rusqlite::params_from_iter(params.iter()), |row| {
            Ok((
                row.get::<_, i64>(1)?,
                row.get::<_, Option<String>>(2)?.unwrap_or_default(),
                row.get::<_, Option<String>>(3)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let mut snippet_order: Vec<i64> = Vec::new();
    let mut snippet_by_sfk: HashMap<i64, Option<String>> = HashMap::new();
    let mut msg_count_by_sfk: HashMap<i64, i64> = HashMap::new();
    for (sfk, _sid, content_text) in &hit_rows {
        let content = content_text.as_deref().unwrap_or("");
        // No occurrence tally here: it only ever feeds the LIKE-density rank
        // term, and `ask` packs with `rank_fn=None`. The reference computes it
        // and then throws it away on this branch.
        *msg_count_by_sfk.entry(*sfk).or_insert(0) += 1;
        if snippet_by_sfk.contains_key(sfk) {
            continue;
        }
        snippet_order.push(*sfk);
        snippet_by_sfk.insert(*sfk, build_snippet(content, needle));
    }

    if snippet_order.is_empty() {
        return Ok(Vec::new());
    }

    let sql = format!(
        "SELECT {SESSION_SELECT}, s.id AS session_fk {SESSION_FROM} \
         WHERE s.id IN ({}) ORDER BY s.last_ts DESC",
        placeholders(snippet_order.len())
    );
    let params: Vec<rusqlite::types::Value> = snippet_order
        .iter()
        .map(|fk| rusqlite::types::Value::Integer(*fk))
        .collect();
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(rusqlite::params_from_iter(params.iter()), |row| {
            let session_fk: i64 = row.get(8)?;
            Ok((session_fk, row_to_match(row)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let mut out: Vec<SessionMatch> = Vec::new();
    for (session_fk, mut session_match) in rows {
        session_match.snippet = snippet_by_sfk.get(&session_fk).cloned().flatten();
        let more = msg_count_by_sfk.get(&session_fk).copied().unwrap_or(1) - 1;
        session_match.more_matches_in_session = (more != 0).then_some(more);
        out.push(session_match);
        if limit > 0 && i64::try_from(out.len()).unwrap_or(i64::MAX) >= limit {
            break;
        }
    }
    Ok(out)
}

/// `cli._hydrate_sessions` — best-effort provenance for bare session ids.
///
/// Never fails: store-shape drift must not break `ask`, so a query error is an
/// empty list, exactly as the reference's bare `except` makes it.
#[must_use]
pub fn hydrate_sessions(
    conn: &Connection,
    session_ids: &[String],
    project_slug: Option<&str>,
) -> Vec<SessionMatch> {
    if session_ids.is_empty() {
        return Vec::new();
    }
    let mut sql = format!(
        "SELECT {SESSION_SELECT} {SESSION_FROM} WHERE s.session_id IN ({})",
        placeholders(session_ids.len())
    );
    let mut params: Vec<rusqlite::types::Value> = session_ids
        .iter()
        .map(|sid| rusqlite::types::Value::Text(sid.clone()))
        .collect();
    if let Some(slug) = project_slug {
        sql.push_str(" AND p.slug = ?");
        params.push(rusqlite::types::Value::Text(slug.to_owned()));
    }
    let Ok(mut stmt) = conn.prepare(&sql) else {
        return Vec::new();
    };
    stmt.query_map(rusqlite::params_from_iter(params.iter()), row_to_match)
        .and_then(Iterator::collect::<rusqlite::Result<Vec<_>>>)
        .unwrap_or_default()
}

/// `cli._fuse_session_ids` — RRF over two session-id orderings, best first.
///
/// A string-keyed twin of [`rrf_merge`]: score is `Σ 1/(k + rank)` over the
/// lists an id appears in, ties break by first-seen order. An empty
/// `hybrid_order` returns `base_order` unchanged.
#[must_use]
pub fn fuse_session_ids(base_order: &[String], hybrid_order: &[String], k: i64) -> Vec<String> {
    let mut scores: HashMap<&str, f64> = HashMap::new();
    let mut first_seen: HashMap<&str, usize> = HashMap::new();
    // CPython iterates `scores` in insertion order; the sort key is a total
    // order (`first_seen` is unique), so only the accumulation order matters —
    // and it must be base-then-hybrid to reproduce the float sums bit for bit.
    let mut order: Vec<&str> = Vec::new();
    let mut seq = 0usize;
    for list in [base_order, hybrid_order] {
        for (rank, sid) in list.iter().enumerate() {
            let sid = sid.as_str();
            let slot = scores.entry(sid).or_insert_with(|| {
                order.push(sid);
                0.0
            });
            *slot += 1.0 / (k as f64 + rank as f64);
            first_seen.entry(sid).or_insert_with(|| {
                let value = seq;
                seq += 1;
                value
            });
        }
    }
    let mut fused: Vec<&str> = order;
    fused.sort_by(|left, right| {
        let (left_score, right_score) = (scores[left], scores[right]);
        right_score
            .total_cmp(&left_score)
            .then_with(|| first_seen[left].cmp(&first_seen[right]))
    });
    fused.into_iter().map(ToOwned::to_owned).collect()
}

/// `cli._hybrid_session_order` — `(session_ids, vector_used)`, best first.
///
/// Deduped, order-preserving. Every failure inside — a missing index, a locked
/// one, an FTS5 syntax hiccup, an unreachable Ollama — degrades to
/// `(vec![], false)`, matching the reference's blanket `except`.
#[must_use]
pub fn hybrid_session_order(
    env: &HybridEnv,
    question: &str,
    project_slug: Option<&str>,
    limit: i64,
) -> (Vec<String>, bool) {
    let hybrid_limit = if limit > 0 { (limit * 3).max(30) } else { 60 };
    let Some(found) = hybrid_search(env, question, project_slug, hybrid_limit) else {
        return (Vec::new(), false);
    };
    let mut seen: Vec<&str> = Vec::new();
    let mut sids: Vec<String> = Vec::new();
    for row in &found.results {
        if row.session_id.is_empty() || seen.contains(&row.session_id.as_str()) {
            continue;
        }
        seen.push(row.session_id.as_str());
        sids.push(row.session_id.clone());
    }
    (sids, found.vector_used)
}

// ── the search index (search_index.db) ───────────────────────────────────────

/// One row of `search_index.db`'s `messages` table — `_rows_for_ids`' shape.
///
/// `content` is not carried: `_rows_for_ids` truncates it to 500 characters for
/// the Search tab, and `ask` reads only `session_id` off these rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexRow {
    /// `messages.id` — the RRF fusion key.
    pub id: i64,
    /// `messages.session_id`.
    pub session_id: String,
    /// `messages.project` — a project slug.
    pub project: String,
    /// `messages.role`.
    pub role: Option<String>,
    /// `messages.timestamp`, ISO-8601.
    pub timestamp: Option<String>,
    /// `messages.model`.
    pub model: Option<String>,
}

/// `SearchService.hybrid_search`'s envelope, reduced to what `ask` consumes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HybridResult {
    /// Fused rows, best first, capped at the caller's limit.
    pub results: Vec<IndexRow>,
    /// Whether the vector half contributed a ranking.
    pub vector_used: bool,
}

/// `SearchService.hybrid_search` — FTS5 `MATCH` fused with a cosine scan.
///
/// `None` where Python returns its `empty` envelope *or* raises out to
/// `_hybrid_session_order`'s `except`; both mean "the hybrid half contributed
/// nothing", and both produce `([], False)` at the call site.
#[must_use]
pub fn hybrid_search(
    env: &HybridEnv,
    query: &str,
    project: Option<&str>,
    limit: i64,
) -> Option<HybridResult> {
    if query.trim().is_empty() {
        return None;
    }
    let index_path = env.index_path.as_deref()?;
    let conn = open_read_only(index_path)?;

    let safe_query = sanitize_fts_query(query);
    let (where_sql, params) = build_filter_clauses(project, None, None, None, None);

    let fts_ids = fts_ranked_ids(&conn, &safe_query, &where_sql, &params, CANDIDATE_K);
    let vector_ids = vector_ranked_ids(env, query, CANDIDATE_K);
    let vector_used = !vector_ids.is_empty();

    let rankings: Vec<&[i64]> = [fts_ids.as_slice(), vector_ids.as_slice()]
        .into_iter()
        .filter(|ranking| !ranking.is_empty())
        .collect();
    if rankings.is_empty() {
        return None;
    }
    let fused = rrf_merge(&rankings, RRF_K, None);
    let fused_ids: Vec<i64> = fused.iter().map(|(id, _)| *id).collect();
    let rows_by_id = rows_for_ids(&conn, &fused_ids);

    // The vector half is not filtered in SQL, so post-filter every fused row
    // against the same predicates. FTS rows already satisfy them.
    let mut results: Vec<IndexRow> = Vec::new();
    for (id, _score) in &fused {
        let Some(row) = rows_by_id.get(id) else {
            continue;
        };
        if !row_matches_filters(row, project, None, None, None, None) {
            continue;
        }
        results.push(row.clone());
        if i64::try_from(results.len()).unwrap_or(i64::MAX) >= limit {
            break;
        }
    }
    Some(HybridResult {
        results,
        vector_used,
    })
}

/// `SearchService._sanitize_fts_query` — every `\w+` run becomes a quoted
/// prefix term, so FTS5 operators reach the engine as literal text.
///
/// Empty / punctuation-only input yields `""`, which matches nothing.
#[must_use]
pub fn sanitize_fts_query(query: &str) -> String {
    let tokens = word_runs(query);
    if tokens.is_empty() {
        return "\"\"".to_owned();
    }
    tokens
        .iter()
        .map(|token| format!("\"{token}\"*"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// `re.findall(r"\w+", text)` — CPython's `\w` is `str.isalnum()` plus `_`.
fn word_runs(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut current = String::new();
    for ch in text.chars() {
        if ch.is_alphanumeric() || ch == '_' {
            current.push(ch);
        } else if !current.is_empty() {
            out.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

/// `SearchService._build_filter_clauses` — `(where_sql, params)`.
///
/// Returns the joined `AND …` fragment rather than the clause list, which is
/// the only form the two call sites use.
#[must_use]
pub fn build_filter_clauses(
    project: Option<&str>,
    date_from: Option<&str>,
    date_to: Option<&str>,
    model: Option<&str>,
    role: Option<&str>,
) -> (String, Vec<String>) {
    let mut clauses: Vec<&str> = Vec::new();
    let mut params: Vec<String> = Vec::new();
    if let Some(project) = project.filter(|value| !value.is_empty()) {
        clauses.push("m.project = ?");
        params.push(project.to_owned());
    }
    if let Some(date_from) = date_from.filter(|value| !value.is_empty()) {
        clauses.push("m.timestamp >= ?");
        params.push(date_from.to_owned());
    }
    if let Some(date_to) = date_to.filter(|value| !value.is_empty()) {
        clauses.push("m.timestamp <= ?");
        params.push(expand_date_to(date_to));
    }
    if let Some(model) = model.filter(|value| !value.is_empty()) {
        clauses.push("m.model = ?");
        params.push(model.to_owned());
    }
    if let Some(role) = role.filter(|value| !value.is_empty()) {
        clauses.push("m.role = ?");
        params.push(role.to_owned());
    }
    let where_sql = if clauses.is_empty() {
        String::new()
    } else {
        format!("AND {}", clauses.join(" AND "))
    };
    (where_sql, params)
}

/// A bare `YYYY-MM-DD` upper bound means end-of-day.
fn expand_date_to(date_to: &str) -> String {
    if date_to.len() == 10 {
        format!("{date_to}T23:59:59")
    } else {
        date_to.to_owned()
    }
}

/// `SearchService._row_matches_filters` — the Python mirror, for vector rows.
#[must_use]
pub fn row_matches_filters(
    row: &IndexRow,
    project: Option<&str>,
    date_from: Option<&str>,
    date_to: Option<&str>,
    model: Option<&str>,
    role: Option<&str>,
) -> bool {
    if let Some(project) = project.filter(|value| !value.is_empty())
        && row.project != project
    {
        return false;
    }
    if let Some(model) = model.filter(|value| !value.is_empty())
        && row.model.as_deref() != Some(model)
    {
        return false;
    }
    if let Some(role) = role.filter(|value| !value.is_empty())
        && row.role.as_deref() != Some(role)
    {
        return false;
    }
    let timestamp = row.timestamp.as_deref().unwrap_or("");
    if let Some(date_from) = date_from.filter(|value| !value.is_empty())
        && timestamp < date_from
    {
        return false;
    }
    if let Some(date_to) = date_to.filter(|value| !value.is_empty())
        && timestamp > expand_date_to(date_to).as_str()
    {
        return false;
    }
    true
}

/// `SearchService._fts_ranked_ids` — message ids, best-relevance first.
///
/// An FTS5 syntax error yields `[]` — the same swallow `search` performs.
#[must_use]
pub fn fts_ranked_ids(
    conn: &Connection,
    safe_query: &str,
    where_sql: &str,
    params: &[String],
    limit: i64,
) -> Vec<i64> {
    let sql = format!(
        "SELECT m.id AS id \
         FROM messages_fts \
         JOIN messages m ON messages_fts.rowid = m.id \
         WHERE messages_fts MATCH ? \
         {where_sql} \
         ORDER BY rank \
         LIMIT ?"
    );
    let mut bound: Vec<rusqlite::types::Value> =
        vec![rusqlite::types::Value::Text(safe_query.to_owned())];
    bound.extend(
        params
            .iter()
            .map(|param| rusqlite::types::Value::Text(param.clone())),
    );
    bound.push(rusqlite::types::Value::Integer(limit));

    let Ok(mut stmt) = conn.prepare(&sql) else {
        return Vec::new();
    };
    stmt.query_map(rusqlite::params_from_iter(bound.iter()), |row| row.get(0))
        .and_then(Iterator::collect::<rusqlite::Result<Vec<i64>>>)
        .unwrap_or_default()
}

/// `SearchService._rows_for_ids` — one `IN (…)` query over the fused set.
#[must_use]
pub fn rows_for_ids(conn: &Connection, ids: &[i64]) -> HashMap<i64, IndexRow> {
    if ids.is_empty() {
        return HashMap::new();
    }
    let sql = format!(
        "SELECT id, session_id, project, role, content, timestamp, \
         model, tokens_input, tokens_output \
         FROM messages \
         WHERE id IN ({})",
        placeholders(ids.len())
    );
    let params: Vec<rusqlite::types::Value> = ids
        .iter()
        .map(|id| rusqlite::types::Value::Integer(*id))
        .collect();
    let Ok(mut stmt) = conn.prepare(&sql) else {
        return HashMap::new();
    };
    let rows = stmt
        .query_map(rusqlite::params_from_iter(params.iter()), |row| {
            Ok(IndexRow {
                id: row.get(0)?,
                session_id: row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                project: row.get::<_, Option<String>>(2)?.unwrap_or_default(),
                role: row.get(3)?,
                timestamp: row.get(5)?,
                model: row.get(6)?,
            })
        })
        .and_then(Iterator::collect::<rusqlite::Result<Vec<_>>>)
        .unwrap_or_default();
    rows.into_iter().map(|row| (row.id, row)).collect()
}

/// `embeddings.rrf_merge` — `Σ 1/(k + rank)`, ties broken by id ascending.
#[must_use]
pub fn rrf_merge(rankings: &[&[i64]], k: i64, limit: Option<usize>) -> Vec<(i64, f64)> {
    let mut scores: HashMap<i64, f64> = HashMap::new();
    for ranking in rankings {
        for (rank, id) in ranking.iter().enumerate() {
            *scores.entry(*id).or_insert(0.0) += 1.0 / (k as f64 + rank as f64);
        }
    }
    let mut merged: Vec<(i64, f64)> = scores.into_iter().collect();
    merged.sort_by(|left, right| {
        right
            .1
            .total_cmp(&left.1)
            .then_with(|| left.0.cmp(&right.0))
    });
    if let Some(limit) = limit {
        merged.truncate(limit);
    }
    merged
}

// ── the vector half (embeddings.db + Ollama) ─────────────────────────────────

/// `SearchService._vector_ranked_ids` — embed the query, cosine-scan, rank.
///
/// `[]` on every miss, in the reference's gate order: resolve the model, count
/// the store's vectors (a store with none short-circuits *before* the network
/// probe), probe Ollama, embed, scan.
#[must_use]
pub fn vector_ranked_ids(env: &HybridEnv, query: &str, candidate_k: i64) -> Vec<i64> {
    if embedding_count(&env.embeddings_path, &env.embed_model) == 0 {
        return Vec::new();
    }
    if !ollama_reachable(&env.probe_url, env.api_key.as_deref()) {
        return Vec::new();
    }
    let Some((base, key)) = env.embed_endpoint.as_ref() else {
        return Vec::new();
    };
    let Some(vector) = embed_one(base, &env.embed_model, query, key.as_deref()) else {
        return Vec::new();
    };
    embedding_search(&env.embeddings_path, &vector, &env.embed_model, candidate_k)
        .into_iter()
        .map(|(id, _)| id)
        .collect()
}

/// `EmbeddingStore.count(model)`.
///
/// `0` when the file is absent — Python's constructor would have created an
/// empty one and counted zero, so the observable answer is the same.
#[must_use]
pub fn embedding_count(path: &Path, model: &str) -> i64 {
    let Some(conn) = open_read_only(path) else {
        return 0;
    };
    conn.query_row(
        "SELECT COUNT(*) AS c FROM embeddings WHERE model = ?",
        [model],
        |row| row.get(0),
    )
    .unwrap_or(0)
}

/// `EmbeddingStore.search` — brute-force cosine, best first, capped at `top_k`.
///
/// Corrupt blobs (`len != dim * 4`) are skipped, never raised, and the sort key
/// is `(-similarity, message_id)` so the output is deterministic.
#[must_use]
pub fn embedding_search(
    path: &Path,
    query_vector: &[f64],
    model: &str,
    top_k: i64,
) -> Vec<(i64, f64)> {
    if query_vector.is_empty() {
        return Vec::new();
    }
    let Some(conn) = open_read_only(path) else {
        return Vec::new();
    };
    let Ok(mut stmt) =
        conn.prepare("SELECT message_id, dim, vector FROM embeddings WHERE model = ?")
    else {
        return Vec::new();
    };
    let mut scored: Vec<(i64, f64)> = Vec::new();
    let Ok(rows) = stmt.query_map([model], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, Vec<u8>>(2)?,
        ))
    }) else {
        return Vec::new();
    };
    for row in rows.flatten() {
        let (message_id, dim, blob) = row;
        let Some(vector) = unpack_vector(&blob, dim) else {
            continue;
        };
        scored.push((message_id, cosine(query_vector, &vector)));
    }
    scored.sort_by(|left, right| {
        right
            .1
            .total_cmp(&left.1)
            .then_with(|| left.0.cmp(&right.0))
    });
    scored.truncate(usize::try_from(top_k).unwrap_or(0));
    scored
}

/// `EmbeddingStore._unpack` — little-endian float32, `None` on a length mismatch.
#[must_use]
pub fn unpack_vector(blob: &[u8], dim: i64) -> Option<Vec<f64>> {
    let dim = usize::try_from(dim).ok()?;
    if blob.len() != dim * 4 {
        return None;
    }
    Some(
        blob.chunks_exact(4)
            .map(|chunk| f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]) as f64)
            .collect(),
    )
}

/// `embeddings.cosine` — `0.0` on a length mismatch or a zero norm.
#[must_use]
pub fn cosine(left: &[f64], right: &[f64]) -> f64 {
    if left.len() != right.len() {
        return 0.0;
    }
    let mut dot = 0.0f64;
    let mut left_norm = 0.0f64;
    let mut right_norm = 0.0f64;
    for (x, y) in left.iter().zip(right.iter()) {
        dot += x * y;
        left_norm += x * x;
        right_norm += y * y;
    }
    if left_norm <= 0.0 || right_norm <= 0.0 {
        return 0.0;
    }
    dot / (left_norm.sqrt() * right_norm.sqrt())
}

/// `embeddings.ollama_reachable` — `GET /api/tags` answers `200` within 1.5 s.
///
/// The per-process reachability cache is not ported: a CLI invocation probes at
/// most once, so the TTL can never be observed.
#[must_use]
pub fn ollama_reachable(base: &str, api_key: Option<&str>) -> bool {
    http_request(base, "/api/tags", None, api_key, REACHABLE_TIMEOUT)
        .is_some_and(|response| response.status == 200)
}

/// `embeddings._embed_one` — `POST /api/embeddings` → the `embedding` array.
///
/// Empty / whitespace-only text is never sent, a non-200 is `None`, and a
/// response without a non-empty `embedding` list is `None`.
#[must_use]
pub fn embed_one(base: &str, model: &str, text: &str, api_key: Option<&str>) -> Option<Vec<f64>> {
    if text.trim().is_empty() {
        return None;
    }
    // The egress chokepoint's allowlist, structurally: exactly these two keys
    // leave the machine (`egress.OLLAMA_EMBED_KEYS`).
    let body = pyjson::dumps_compact(&pyjson::Value::Object(vec![
        ("model".to_owned(), pyjson::Value::Str(model.to_owned())),
        ("prompt".to_owned(), pyjson::Value::Str(text.to_owned())),
    ]));
    let response = http_request(base, "/api/embeddings", Some(&body), api_key, EMBED_TIMEOUT)?;
    if response.status != 200 {
        return None;
    }
    let parsed = pyjson::loads(&String::from_utf8(response.body).ok()?)?;
    let pyjson::Value::Array(items) = parsed.get("embedding")? else {
        return None;
    };
    if items.is_empty() {
        return None;
    }
    items
        .iter()
        .map(|item| match item {
            pyjson::Value::Float(value) => Some(*value),
            pyjson::Value::Int(value) => Some(*value as f64),
            _ => None,
        })
        .collect()
}

// ── plumbing ─────────────────────────────────────────────────────────────────

/// Open a sidecar database strictly read-only; `None` when it is not there.
///
/// Python opens `search_index.db` and `embeddings.db` read-write and *creates*
/// them (`SearchService.__init__` even applies the schema). This port never
/// writes a byte outside the store, which is the wave-0 decision restated: a
/// query command does not create databases.
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

/// The pieces of an HTTP response this module reads.
struct HttpResponse {
    status: u16,
    body: Vec<u8>,
}

/// A minimal HTTP/1.1 round trip — `GET` when `body` is `None`, else `POST`.
///
/// Hand-rolled rather than pulled in as a dependency: the whole surface is two
/// requests to a localhost daemon. `https://` bases are unsupported and read as
/// unreachable, which only affects a hosted-Ollama configuration (recorded as a
/// divergence); every default and every test endpoint is plain HTTP.
fn http_request(
    base: &str,
    path: &str,
    body: Option<&str>,
    api_key: Option<&str>,
    timeout: Duration,
) -> Option<HttpResponse> {
    let (host, port, prefix) = split_http_base(base)?;
    let address = (host.as_str(), port).to_socket_addrs().ok()?.next()?;
    let mut stream = TcpStream::connect_timeout(&address, timeout).ok()?;
    stream.set_read_timeout(Some(timeout)).ok()?;
    stream.set_write_timeout(Some(timeout)).ok()?;

    let method = if body.is_some() { "POST" } else { "GET" };
    let mut request = format!(
        "{method} {prefix}{path} HTTP/1.1\r\nHost: {host}:{port}\r\n\
         Accept: */*\r\nConnection: close\r\n"
    );
    if let Some(key) = api_key {
        request.push_str(&format!("Authorization: Bearer {key}\r\n"));
    }
    if let Some(body) = body {
        request.push_str("Content-Type: application/json\r\n");
        request.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    request.push_str("\r\n");
    if let Some(body) = body {
        request.push_str(body);
    }
    stream.write_all(request.as_bytes()).ok()?;
    stream.flush().ok()?;

    let mut raw: Vec<u8> = Vec::new();
    stream.read_to_end(&mut raw).ok()?;
    parse_http_response(&raw)
}

/// `http://host[:port][/prefix]` → `(host, port, prefix)`; `None` otherwise.
fn split_http_base(base: &str) -> Option<(String, u16, String)> {
    let rest = base.strip_prefix("http://")?;
    let (authority, prefix) = match rest.find('/') {
        Some(index) => (&rest[..index], rest[index..].trim_end_matches('/')),
        None => (rest, ""),
    };
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) => (host, port.parse().ok()?),
        None => (authority, 80u16),
    };
    if host.is_empty() {
        return None;
    }
    Some((host.to_owned(), port, prefix.to_owned()))
}

/// Split the status line and body out of a raw response, de-chunking if needed.
fn parse_http_response(raw: &[u8]) -> Option<HttpResponse> {
    let split = raw.windows(4).position(|window| window == b"\r\n\r\n")?;
    let head = std::str::from_utf8(&raw[..split]).ok()?;
    let mut lines = head.split("\r\n");
    let status = lines.next()?.split_whitespace().nth(1)?.parse().ok()?;
    let chunked = lines.any(|line| {
        let lower = line.to_ascii_lowercase();
        lower.starts_with("transfer-encoding:") && lower.contains("chunked")
    });
    let body = &raw[split + 4..];
    Some(HttpResponse {
        status,
        body: if chunked {
            decode_chunked(body)?
        } else {
            body.to_vec()
        },
    })
}

/// `Transfer-Encoding: chunked` → the concatenated payload.
fn decode_chunked(body: &[u8]) -> Option<Vec<u8>> {
    let mut out: Vec<u8> = Vec::new();
    let mut rest = body;
    loop {
        let line_end = rest.windows(2).position(|window| window == b"\r\n")?;
        let size_line = std::str::from_utf8(&rest[..line_end]).ok()?;
        let size = usize::from_str_radix(size_line.split(';').next()?.trim(), 16).ok()?;
        rest = &rest[line_end + 2..];
        if size == 0 {
            return Some(out);
        }
        if rest.len() < size {
            return None;
        }
        out.extend_from_slice(&rest[..size]);
        rest = rest.get(size + 2..)?;
    }
}

#[cfg(test)]
mod tests {
    use std::io::BufRead;
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;

    // ── a scratch directory that cleans up after itself ──────────────────────

    struct Scratch {
        root: PathBuf,
    }

    impl Scratch {
        fn new(tag: &str) -> Self {
            static COUNTER: AtomicU32 = AtomicU32::new(0);
            let root = std::env::temp_dir().join(format!(
                "stax-ask-{}-{}-{}",
                tag,
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&root).expect("creating the scratch directory");
            Self { root }
        }

        fn path(&self, name: &str) -> PathBuf {
            self.root.join(name)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    /// A `search_index.db` with the reference's schema and `rows` indexed.
    fn build_index(path: &Path, rows: &[(i64, &str, &str, &str, &str)]) {
        let conn = Connection::open(path).expect("creating the index");
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
        .expect("applying the index schema");
        for (id, session_id, project, content, timestamp) in rows {
            conn.execute(
                "INSERT INTO messages (id, session_id, project, role, content, timestamp) \
                 VALUES (?, ?, ?, 'assistant', ?, ?)",
                rusqlite::params![id, session_id, project, content, timestamp],
            )
            .expect("indexing a message");
        }
    }

    /// An `embeddings.db` holding `rows` of `(message_id, vector)`.
    fn build_embeddings(path: &Path, model: &str, rows: &[(i64, Vec<f32>)]) {
        let conn = Connection::open(path).expect("creating the vector store");
        conn.execute_batch(
            "CREATE TABLE embeddings (
                 message_id INTEGER NOT NULL,
                 model      TEXT    NOT NULL,
                 dim        INTEGER NOT NULL,
                 vector     BLOB    NOT NULL,
                 PRIMARY KEY (message_id, model)
             );",
        )
        .expect("applying the vector schema");
        for (message_id, vector) in rows {
            let blob: Vec<u8> = vector
                .iter()
                .flat_map(|value| value.to_le_bytes())
                .collect();
            conn.execute(
                "INSERT INTO embeddings (message_id, model, dim, vector) VALUES (?, ?, ?, ?)",
                rusqlite::params![message_id, model, vector.len() as i64, blob],
            )
            .expect("storing a vector");
        }
    }

    /// A one-shot Ollama stand-in: answers `/api/tags` 200 and `/api/embeddings`
    /// with `embedding`. Returns its base URL and the join handle.
    fn fake_ollama(vector: Vec<f32>, requests: usize) -> (String, std::thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("binding the fake daemon");
        let base = format!("http://{}", listener.local_addr().expect("its address"));
        let handle = std::thread::spawn(move || {
            for _ in 0..requests {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                let mut reader = std::io::BufReader::new(
                    stream.try_clone().expect("cloning the accepted socket"),
                );
                let mut request_line = String::new();
                let _ = reader.read_line(&mut request_line);
                // Drain the headers so the client's write completes.
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                }
                let body = if request_line.contains("/api/embeddings") {
                    let numbers: Vec<String> =
                        vector.iter().map(|value| format!("{value:?}")).collect();
                    format!("{{\"embedding\":[{}]}}", numbers.join(","))
                } else {
                    "{\"models\":[]}".to_owned()
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
            }
        });
        (base, handle)
    }

    // ── the query sanitiser ─────────────────────────────────────────────────

    #[test]
    fn every_word_run_becomes_a_quoted_prefix_term() {
        assert_eq!(sanitize_fts_query("cache"), "\"cache\"*");
        assert_eq!(
            sanitize_fts_query("how does caching work"),
            "\"how\"* \"does\"* \"caching\"* \"work\"*"
        );
    }

    #[test]
    fn fts5_operators_survive_as_literal_text() {
        // The whole point: `NOT` must not reach the FTS5 parser as an operator.
        assert_eq!(
            sanitize_fts_query("use NOT null"),
            "\"use\"* \"NOT\"* \"null\"*"
        );
        assert_eq!(sanitize_fts_query("a-b"), "\"a\"* \"b\"*");
        assert_eq!(sanitize_fts_query("\"unbalanced"), "\"unbalanced\"*");
        assert_eq!(sanitize_fts_query("*"), "\"\"");
        assert_eq!(sanitize_fts_query("!!!"), "\"\"");
        assert_eq!(sanitize_fts_query(""), "\"\"");
    }

    #[test]
    fn unicode_word_characters_count_as_terms() {
        assert_eq!(sanitize_fts_query("café_naïve"), "\"café_naïve\"*");
        assert_eq!(
            sanitize_fts_query("日本語 テスト"),
            "\"日本語\"* \"テスト\"*"
        );
    }

    // ── fusion ──────────────────────────────────────────────────────────────

    #[test]
    fn rrf_merge_scores_a_single_ranking_in_its_own_order() {
        let merged = rrf_merge(&[&[7, 3, 9]], 60, None);
        assert_eq!(
            merged.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
            vec![7, 3, 9]
        );
        assert!((merged[0].1 - 1.0 / 60.0).abs() < f64::EPSILON);
    }

    #[test]
    fn rrf_merge_breaks_ties_by_id_ascending() {
        // Both lead their own list, so both score exactly 1/60.
        let merged = rrf_merge(&[&[9], &[4]], 60, None);
        assert_eq!(
            merged.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
            vec![4, 9]
        );
    }

    #[test]
    fn an_id_in_both_rankings_outscores_one_in_either() {
        let merged = rrf_merge(&[&[1, 2], &[2, 3]], 60, None);
        assert_eq!(merged[0].0, 2, "2 appears in both lists");
        assert_eq!(merged.len(), 3);
        assert_eq!(rrf_merge(&[&[1, 2], &[2, 3]], 60, Some(2)).len(), 2);
    }

    #[test]
    fn fusing_with_an_empty_hybrid_order_is_the_identity() {
        let base: Vec<String> = ["a", "b", "c"].iter().map(|s| (*s).to_owned()).collect();
        assert_eq!(fuse_session_ids(&base, &[], 60), base);
    }

    #[test]
    fn session_fusion_breaks_ties_by_first_seen_not_by_id() {
        let base = vec!["zzz".to_owned()];
        let hybrid = vec!["aaa".to_owned()];
        // Both score 1/60; `zzz` was seen first, so `zzz` leads — the opposite
        // of `rrf_merge`'s id-ascending rule, and deliberately so.
        assert_eq!(
            fuse_session_ids(&base, &hybrid, 60),
            vec!["zzz".to_owned(), "aaa".to_owned()]
        );
    }

    #[test]
    fn a_session_in_both_orders_is_promoted_over_either_leader() {
        let base = vec!["only-base".to_owned(), "both".to_owned()];
        let hybrid = vec!["only-hybrid".to_owned(), "both".to_owned()];
        assert_eq!(fuse_session_ids(&base, &hybrid, 60)[0], "both");
    }

    // ── filters ─────────────────────────────────────────────────────────────

    #[test]
    fn filter_clauses_match_the_reference_sql() {
        let (where_sql, params) = build_filter_clauses(Some("-p"), None, None, None, None);
        assert_eq!(where_sql, "AND m.project = ?");
        assert_eq!(params, vec!["-p".to_owned()]);

        let (where_sql, params) = build_filter_clauses(
            Some("-p"),
            Some("2026-01-01"),
            Some("2026-01-31"),
            None,
            None,
        );
        assert_eq!(
            where_sql,
            "AND m.project = ? AND m.timestamp >= ? AND m.timestamp <= ?"
        );
        assert_eq!(
            params[2], "2026-01-31T23:59:59",
            "a bare date means end-of-day"
        );

        assert_eq!(build_filter_clauses(None, None, None, None, None).0, "");
    }

    #[test]
    fn row_filters_mirror_the_sql_ones() {
        let row = IndexRow {
            id: 1,
            session_id: "s".into(),
            project: "-p".into(),
            role: Some("assistant".into()),
            timestamp: Some("2026-01-15T00:00:00".into()),
            model: Some("opus".into()),
        };
        assert!(row_matches_filters(
            &row,
            Some("-p"),
            None,
            None,
            None,
            None
        ));
        assert!(!row_matches_filters(
            &row,
            Some("-q"),
            None,
            None,
            None,
            None
        ));
        assert!(row_matches_filters(
            &row,
            None,
            Some("2026-01-01"),
            Some("2026-01-31"),
            None,
            None
        ));
        assert!(!row_matches_filters(
            &row,
            None,
            Some("2026-02-01"),
            None,
            None,
            None
        ));
        assert!(!row_matches_filters(
            &row,
            None,
            None,
            Some("2026-01-14"),
            None,
            None
        ));
        assert!(!row_matches_filters(
            &row,
            None,
            None,
            None,
            Some("sonnet"),
            None
        ));
        assert!(!row_matches_filters(
            &row,
            None,
            None,
            None,
            None,
            Some("user")
        ));
    }

    // ── the FTS half against a real FTS5 index ──────────────────────────────

    #[test]
    fn the_bm25_ranking_comes_back_best_first() {
        let scratch = Scratch::new("fts");
        let index = scratch.path("search_index.db");
        build_index(
            &index,
            &[
                (
                    1,
                    "s-a",
                    "-p",
                    "cache cache cache invalidation",
                    "2026-01-01T00:00:00",
                ),
                (
                    2,
                    "s-b",
                    "-p",
                    "a passing mention of cache",
                    "2026-01-02T00:00:00",
                ),
                (
                    3,
                    "s-c",
                    "-other",
                    "cache in another project",
                    "2026-01-03T00:00:00",
                ),
            ],
        );
        let conn = open_read_only(&index).expect("opening the index");
        let (where_sql, params) = build_filter_clauses(Some("-p"), None, None, None, None);
        let ids = fts_ranked_ids(&conn, &sanitize_fts_query("cache"), &where_sql, &params, 50);
        assert_eq!(ids, vec![1, 2], "the project filter drops row 3");

        let unfiltered = fts_ranked_ids(&conn, &sanitize_fts_query("cache"), "", &[], 50);
        assert_eq!(unfiltered.len(), 3);
        assert_eq!(unfiltered[0], 1, "the densest match ranks first");
    }

    #[test]
    fn a_missing_index_is_simply_no_contribution() {
        let mut env = HybridEnv::disabled();
        env.index_path = Some(PathBuf::from("/nonexistent/search_index.db"));
        assert_eq!(
            hybrid_session_order(&env, "cache", None, 20),
            (vec![], false)
        );
    }

    #[test]
    fn an_unpopulated_index_is_no_contribution_either() {
        let scratch = Scratch::new("empty");
        let index = scratch.path("search_index.db");
        build_index(&index, &[]);
        let mut env = HybridEnv::disabled();
        env.index_path = Some(index);
        assert_eq!(
            hybrid_session_order(&env, "cache", None, 20),
            (vec![], false)
        );
    }

    #[test]
    fn sessions_come_back_deduped_in_bm25_order() {
        let scratch = Scratch::new("dedup");
        let index = scratch.path("search_index.db");
        build_index(
            &index,
            &[
                (1, "s-a", "-p", "cache cache cache", "2026-01-01T00:00:00"),
                (2, "s-a", "-p", "cache again", "2026-01-02T00:00:00"),
                (3, "s-b", "-p", "cache once", "2026-01-03T00:00:00"),
            ],
        );
        let mut env = HybridEnv::disabled();
        env.index_path = Some(index);
        let (sids, vector_used) = hybrid_session_order(&env, "cache", Some("-p"), 20);
        assert_eq!(sids, vec!["s-a".to_owned(), "s-b".to_owned()]);
        assert!(!vector_used, "no daemon, no vector half");
    }

    #[test]
    fn a_phrase_that_zeroes_on_like_still_matches_on_fts() {
        // Finding 3: `LIKE '%cache lookup%'` needs the words adjacent; bm25 does
        // not. This is exactly the ordering signal `ask` gains from the index.
        let scratch = Scratch::new("phrase");
        let index = scratch.path("search_index.db");
        build_index(
            &index,
            &[(
                1,
                "s-a",
                "-p",
                "the cache is consulted before the lookup",
                "2026-01-01T00:00:00",
            )],
        );
        let mut env = HybridEnv::disabled();
        env.index_path = Some(index);
        assert_eq!(
            hybrid_session_order(&env, "cache lookup", Some("-p"), 20).0,
            vec!["s-a".to_owned()]
        );
    }

    // ── the vector half ─────────────────────────────────────────────────────

    #[test]
    fn cosine_matches_the_reference_edge_cases() {
        assert!((cosine(&[1.0, 0.0], &[1.0, 0.0]) - 1.0).abs() < 1e-12);
        assert!(cosine(&[1.0, 0.0], &[0.0, 1.0]).abs() < 1e-12);
        assert_eq!(cosine(&[1.0], &[1.0, 2.0]), 0.0, "length mismatch is 0.0");
        assert_eq!(cosine(&[0.0, 0.0], &[1.0, 1.0]), 0.0, "a zero norm is 0.0");
    }

    #[test]
    fn vectors_unpack_as_little_endian_float32() {
        let blob: Vec<u8> = [1.5f32, -2.0]
            .iter()
            .flat_map(|value| value.to_le_bytes())
            .collect();
        assert_eq!(unpack_vector(&blob, 2), Some(vec![1.5, -2.0]));
        assert_eq!(unpack_vector(&blob, 3), None, "a corrupt blob is skipped");
    }

    #[test]
    fn an_empty_vector_store_short_circuits_before_the_probe() {
        let scratch = Scratch::new("novec");
        let store = scratch.path("embeddings.db");
        build_embeddings(&store, "other-model", &[(1, vec![1.0, 0.0])]);
        let mut env = HybridEnv::disabled();
        env.embeddings_path = store;
        // The store has vectors, but none for *this* model — count is 0, so the
        // gate returns before it ever opens a socket.
        assert_eq!(embedding_count(&env.embeddings_path, &env.embed_model), 0);
        assert!(vector_ranked_ids(&env, "cache", 50).is_empty());
    }

    #[test]
    fn a_dead_ollama_port_means_no_vector_half() {
        // The reference's own hermetic-test contract: every endpoint resolves to
        // a dead port so a developer's running daemon cannot decide the outcome.
        let scratch = Scratch::new("dead");
        let store = scratch.path("embeddings.db");
        build_embeddings(&store, DEFAULT_EMBED_MODEL, &[(1, vec![1.0, 0.0])]);
        let mut env = HybridEnv::disabled();
        env.embeddings_path = store;
        assert_eq!(embedding_count(&env.embeddings_path, &env.embed_model), 1);
        assert!(!ollama_reachable(&env.probe_url, None));
        assert!(vector_ranked_ids(&env, "cache", 50).is_empty());
    }

    #[test]
    fn a_reachable_ollama_turns_the_vector_half_on() {
        let scratch = Scratch::new("live");
        let store = scratch.path("embeddings.db");
        build_embeddings(
            &store,
            DEFAULT_EMBED_MODEL,
            &[(1, vec![0.0, 1.0]), (2, vec![1.0, 0.0])],
        );
        // Three requests: the assertion's probe below, then the gate's own
        // probe and its embed call.
        let (base, handle) = fake_ollama(vec![1.0, 0.0], 3);
        let mut env = HybridEnv::disabled();
        env.embeddings_path = store;
        env.probe_url = base.clone();
        env.embed_endpoint = Some((base, None));

        assert!(ollama_reachable(&env.probe_url, None));
        let ranked = vector_ranked_ids(&env, "cache", 50);
        assert_eq!(ranked, vec![2, 1], "message 2's vector is the query's");
        handle.join().expect("the fake daemon exits cleanly");
    }

    #[test]
    fn a_semantic_only_hit_reaches_the_fused_order() {
        let scratch = Scratch::new("semantic");
        let index = scratch.path("search_index.db");
        // Row 2 does not contain the query word at all — only the vector half
        // can surface it, which is the whole promise of the hybrid path.
        build_index(
            &index,
            &[
                (
                    1,
                    "s-keyword",
                    "-p",
                    "cache invalidation",
                    "2026-01-01T00:00:00",
                ),
                (
                    2,
                    "s-semantic",
                    "-p",
                    "memoised lookups",
                    "2026-01-02T00:00:00",
                ),
            ],
        );
        let store = scratch.path("embeddings.db");
        build_embeddings(
            &store,
            DEFAULT_EMBED_MODEL,
            &[(1, vec![0.0, 1.0]), (2, vec![1.0, 0.0])],
        );
        let (base, handle) = fake_ollama(vec![1.0, 0.0], 2);
        let mut env = HybridEnv::disabled();
        env.index_path = Some(index);
        env.embeddings_path = store;
        env.probe_url = base.clone();
        env.embed_endpoint = Some((base, None));

        let (sids, vector_used) = hybrid_session_order(&env, "cache", Some("-p"), 20);
        assert!(vector_used, "the semantic half contributed");
        // RRF, not cosine: message 1 is in both rankings (1/60 + 1/61) and so
        // outscores message 2's lone top slot (1/60) even though message 2 is
        // the exact vector match. The win is that `s-semantic` is *present* at
        // all — the LIKE base could never have found it.
        assert_eq!(sids, vec!["s-keyword".to_owned(), "s-semantic".to_owned()]);
        handle.join().expect("the fake daemon exits cleanly");
    }

    // ── endpoint resolution ─────────────────────────────────────────────────

    #[test]
    fn the_default_endpoints_are_local_and_unauthenticated() {
        let env = HybridEnv::resolve(
            Path::new("/data/su"),
            Some(Path::new("/data/su/store.db")),
            None,
            None,
            None,
            None,
        );
        assert_eq!(env.embed_model, DEFAULT_EMBED_MODEL);
        assert_eq!(env.probe_url, DEFAULT_OLLAMA_URL);
        assert_eq!(
            env.embed_endpoint,
            Some((DEFAULT_OLLAMA_URL.to_owned(), None))
        );
        assert_eq!(env.api_key, None);
        assert_eq!(
            env.index_path,
            Some(PathBuf::from("/data/su/search_index.db"))
        );
        assert_eq!(env.embeddings_path, PathBuf::from("/data/su/embeddings.db"));
    }

    #[test]
    fn the_cloud_url_moves_the_embed_endpoint_but_not_the_probe() {
        // Bug-for-bug: `_vector_ranked_ids` probes `_resolve_url(None)` (local)
        // and embeds through `_resolve_endpoints()[0]` (cloud). A box with only
        // STACKUNDERFLOW_OLLAMA_URL set therefore gates on a daemon it will
        // never call.
        let env = HybridEnv::resolve(
            Path::new("/data/su"),
            None,
            None,
            None,
            Some("https://ollama.example.com/"),
            Some("secret"),
        );
        assert_eq!(env.probe_url, DEFAULT_OLLAMA_URL);
        assert_eq!(
            env.embed_endpoint,
            Some((
                "https://ollama.example.com".to_owned(),
                Some("secret".to_owned())
            ))
        );
        assert_eq!(env.index_path, None, "no store path, no derived index");
    }

    #[test]
    fn an_explicit_local_ollama_url_is_not_duplicated() {
        let env = HybridEnv::resolve(
            Path::new("/data/su"),
            None,
            Some("custom-model"),
            Some(DEFAULT_OLLAMA_URL),
            None,
            None,
        );
        assert_eq!(env.embed_model, "custom-model");
        assert_eq!(
            env.embed_endpoint,
            Some((DEFAULT_OLLAMA_URL.to_owned(), None))
        );
    }

    #[test]
    fn empty_environment_values_count_as_unset() {
        let env = HybridEnv::resolve(
            Path::new("/data/su"),
            None,
            Some(""),
            Some(""),
            Some(""),
            Some(""),
        );
        assert_eq!(env.embed_model, DEFAULT_EMBED_MODEL);
        assert_eq!(env.probe_url, DEFAULT_OLLAMA_URL);
        assert_eq!(env.api_key, None);
    }

    // ── HTTP plumbing ───────────────────────────────────────────────────────

    #[test]
    fn http_bases_split_into_host_port_and_prefix() {
        assert_eq!(
            split_http_base("http://localhost:11434"),
            Some(("localhost".to_owned(), 11434, String::new()))
        );
        assert_eq!(
            split_http_base("http://example.com/ollama/"),
            Some(("example.com".to_owned(), 80, "/ollama".to_owned()))
        );
        assert_eq!(split_http_base("https://example.com"), None, "no TLS here");
        assert_eq!(split_http_base("not-a-url"), None);
    }

    #[test]
    fn chunked_responses_are_reassembled() {
        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n0\r\n\r\n";
        let response = parse_http_response(raw).expect("a parseable response");
        assert_eq!(response.status, 200);
        assert_eq!(response.body, b"hello");
    }

    #[test]
    fn a_non_200_embed_response_is_no_vector() {
        assert_eq!(embed_one("http://127.0.0.1:1", "m", "text", None), None);
        assert_eq!(
            embed_one("http://127.0.0.1:1", "m", "   ", None),
            None,
            "blank text is never sent"
        );
    }
}
