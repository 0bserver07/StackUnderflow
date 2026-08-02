//! `routes/optimize.py` — 3 endpoints, wave 5.
//!
//! | Item | Method | FastAPI path | axum path | State |
//! |---|---|---|---|---|
//! | `RS-5-085` | `GET ` | `/api/optimize                 ` | `/api/optimize` | **ported** |
//! | `RS-5-086` | `GET ` | `/api/optimize/prescriptions   ` | `/api/optimize/prescriptions` | **ported** |
//! | `RS-5-087` | `POST` | `/api/optimize/claudemd-preview` | `/api/optimize/claudemd-preview` | **ported** |
//!
//! # `GET /api/optimize`
//!
//! Seven waste detectors plus the legacy looped-Q&A view plus cost-anomaly
//! detection, over a `period=` window. All of it lives in
//! [`crate::services::optimize`] and [`crate::services::anomaly`]; this module
//! is the thin shell Python's is — validate, call, assemble, return.
//!
//! ```text
//! {"scope", "waste", "patterns", "total_waste_usd", "anomalies", "warnings", "cache"}
//! ```
//!
//! # The cache IS the response, so it had to be ported (DIV-111)
//!
//! `routes/data.rs` dropped its LRU as DIV-055 on the grounds that a memo can
//! only ever return the same answer. That reasoning does not survive contact
//! with this endpoint: the body carries `"cache": "hit" | "miss"`, so the memo
//! is an *answer-changer* and dropping it would be a guaranteed byte divergence
//! on the second identical request. So [`OptimizeCache`] reproduces the Python
//! `dict` faithfully — insertion-ordered, FIFO-trimmed at 16, keyed on
//! `(period, sorted(project), sorted(exclude))` plus `store.db`'s
//! `st_mtime_ns`, with `?force=true` bypassing the read but still writing back.
//!
//! Two consequences worth stating out loud:
//!
//! * `?project=b&project=a` and `?project=a&project=b` are the SAME key, because
//!   Python sorts the tuple. They are not the same key for the *detectors* — the
//!   filter is applied unsorted downstream — but the results are identical, so
//!   the collision is benign and inherited.
//! * The mtime is a *validity token*, not part of the key: a stale entry is
//!   found, rejected, and left in place until something overwrites it.
//!
//! # `schema.apply` is not ported
//!
//! Python opens its own connection and runs `schema.apply(conn)` per request —
//! a migration on a GET. `apply` reads `PRAGMA user_version` and returns
//! immediately when the store is current, which it always is by the time a
//! request arrives (`server.py`'s lifespan applied it at startup). The port
//! never migrates a store it is only reading. Same call, same reasoning, as
//! DIV-085 in `routes/compare.rs`.
//!
//! # LAW 7 clearance
//!
//! Neither ported path writes anything: the detectors only `is_file` /
//! `is_dir` / `read_dir` / `read_text` under `~/.claude`, the pricing engine
//! only reads `models.toml`, and the only mutable state is this module's
//! in-process cache. Verified against the sources rather than the docstring —
//! DIV-118. So `/api/optimize` is case-row-safe.

use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use axum::Router;
use axum::body::Bytes;
use axum::extract::{RawQuery, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use rusqlite::Connection;
use serde_json::{Map, Value};
use stax_etl::stats::aggregator::{Neumaier, PyNum};

use crate::currency::active_currency_payload;
use crate::json::{HandlerResult, HttpError, JsonBody, join_failure, validation_422};
use crate::qs::Query;
use crate::services::optimize::{FsRoots, round_half_even};
use crate::services::prescribe::{
    DEFAULT_SESSIONS_PER_MONTH, build_routing_recommendations, generate_claudemd_preview,
    routing_candidates,
};
use crate::services::scope::{Instant, Scope, parse_period};
use crate::services::{anomaly, mart_queries, optimize};
use crate::state::AppState;

/// `MAX_CLAUDEMD_BYTES = 2_000_000` — two orders of magnitude over a real
/// CLAUDE.md, which keeps the pure-text diff/parse work bounded.
const MAX_CLAUDEMD_BYTES: usize = 2_000_000;

/// `_VALID_PERIODS`, and the `", ".join(sorted(...))` the 400 message prints.
///
/// Listed in SORTED order because the error message is
/// `', '.join(sorted(_VALID_PERIODS))` — over a `set`, so the sort is what makes
/// it deterministic, and `"30days"` sorts before `"7days"` (`'3' < '7'`).
const VALID_PERIODS: [&str; 5] = ["30days", "7days", "all", "month", "today"];

/// `_OPTIMIZE_CACHE_MAX = 16` — tiny LRU; the params space is small in practice.
const OPTIMIZE_CACHE_MAX: usize = 16;

/// Mount this module's endpoints onto `router`.
///
/// Called once, from [`super::register_all`], at this module's
/// `include_router` position.
pub fn register(router: Router<AppState>) -> Router<AppState> {
    router
        .route("/api/optimize", get(get_optimize_report))
        .route("/api/optimize/prescriptions", get(get_prescriptions))
        .route(
            "/api/optimize/claudemd-preview",
            post(post_claudemd_preview),
        )
}

// ── the in-process cache ─────────────────────────────────────────────────────

/// `(period, tuple(sorted(project)) or (), tuple(sorted(exclude)) or ())`.
type CacheKey = (String, Vec<String>, Vec<String>);

/// One `_OPTIMIZE_CACHE` entry: `key -> (mtime, payload)`.
#[derive(Debug, Clone)]
struct CacheEntry {
    key: CacheKey,
    mtime: i64,
    payload: Value,
}

/// `_OPTIMIZE_CACHE` + `_OPTIMIZE_CACHE_LOCK`.
///
/// A `Vec` and not a `HashMap`, because the eviction rule is *insertion order*
/// (`next(iter(dict))` is the oldest-inserted key) and a hash map has no such
/// notion. Sixteen entries makes the linear scan free.
#[derive(Debug, Default)]
struct OptimizeCache {
    entries: Vec<CacheEntry>,
}

impl OptimizeCache {
    /// `_cache_get(key, mtime)`.
    ///
    /// A stale entry (mtime moved) is reported as a miss and **left in place** —
    /// Python does not evict it here, and the next `_cache_put` overwrites it.
    fn get(&self, key: &CacheKey, mtime: i64) -> Option<Value> {
        let hit = self.entries.iter().find(|e| &e.key == key)?;
        if hit.mtime != mtime {
            return None;
        }
        Some(hit.payload.clone())
    }

    /// `_cache_put(key, mtime, payload)`.
    ///
    /// The trim runs FIRST and keys on length alone, so re-writing a key that is
    /// already present at len == 16 still evicts the oldest — and when the key
    /// being written *is* the oldest, it is evicted and then re-appended at the
    /// tail. Both are Python `dict` semantics and both are reproduced.
    fn put(&mut self, key: CacheKey, mtime: i64, payload: Value) {
        if self.entries.len() >= OPTIMIZE_CACHE_MAX {
            self.entries.remove(0);
        }
        // `_OPTIMIZE_CACHE[key] = …` on an existing key keeps its position.
        if let Some(slot) = self.entries.iter_mut().find(|e| e.key == key) {
            slot.mtime = mtime;
            slot.payload = payload;
        } else {
            self.entries.push(CacheEntry {
                key,
                mtime,
                payload,
            });
        }
    }
}

/// The process-wide cache — Python's module-level `dict`.
fn cache() -> &'static Mutex<OptimizeCache> {
    static CACHE: OnceLock<Mutex<OptimizeCache>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(OptimizeCache::default()))
}

/// `_store_mtime_ns()` — `store.db`'s mtime in nanoseconds, or `0` when missing.
fn store_mtime_ns(path: &std::path::Path) -> i64 {
    let Ok(meta) = std::fs::metadata(path) else {
        return 0;
    };
    let Ok(modified) = meta.modified() else {
        return 0;
    };
    modified
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_nanos()).ok())
        .unwrap_or(0)
}

// ── GET /api/optimize ────────────────────────────────────────────────────────

/// `get_optimize_report(period, project, exclude, force)`.
///
/// Declared `async def` in Python but the body is entirely blocking (sqlite,
/// the filesystem sweep, the pricing engine), so it runs on `spawn_blocking`
/// here rather than parking the event loop for the length of a mart scan.
async fn get_optimize_report(
    State(state): State<AppState>,
    RawQuery(raw): RawQuery,
) -> HandlerResult {
    let query = Query::parse(raw.as_deref().unwrap_or_default());
    let period = query.str_or("period", "30days").to_owned();
    let project = query.opt_list("project");
    let exclude = query.opt_list("exclude");
    let force = match query.bool_or("force", false) {
        Ok(value) => value,
        Err(err) => return Ok(validation_422(&err)),
    };

    if !VALID_PERIODS.contains(&period.as_str()) {
        // `HTTPException(400, f"Unknown period '{period}'. Valid: {', '.join(sorted(...))}")`
        return Err(HttpError::bad_request(format!(
            "Unknown period '{period}'. Valid: {}",
            VALID_PERIODS.join(", ")
        )));
    }

    // `tuple(sorted(project)) if project else ()` — a present-but-empty list is
    // falsy in Python and collapses to the empty tuple, same as absent.
    let key: CacheKey = (
        period.clone(),
        sorted_or_empty(project.as_deref()),
        sorted_or_empty(exclude.as_deref()),
    );
    let mtime = store_mtime_ns(state.store_path());

    if !force {
        let cached = cache()
            .lock()
            .map_err(|err| {
                HttpError::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("cache lock: {err}"),
                )
            })?
            .get(&key, mtime);
        if let Some(mut payload) = cached {
            // `cached = dict(cached); cached["cache"] = "hit"` — the key already
            // exists, so the assignment keeps its position (last).
            if let Value::Object(map) = &mut payload {
                map.insert("cache".to_owned(), Value::from("hit"));
            }
            return Ok(JsonBody::ok(payload));
        }
    }

    let scope = parse_period(&period, Instant::now_utc())
        // Unreachable: the allow-list above already rejected everything
        // `parse_period` would raise on. Ported as defence in depth.
        .map_err(HttpError::bad_request)?;

    let worker = state.clone();
    let worker_project = project.clone();
    let worker_exclude = exclude.clone();
    let payload = tokio::task::spawn_blocking(move || {
        compute_optimize(
            &worker,
            &scope,
            worker_project.as_deref(),
            worker_exclude.as_deref(),
        )
    })
    .await
    .map_err(|err| join_failure(&err))??;

    // `_cache_put(key, mtime, payload)` — runs on the `force` path too, so a
    // forced call still warms the cache for the next one.
    cache()
        .lock()
        .map_err(|err| {
            HttpError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("cache lock: {err}"),
            )
        })?
        .put(key, mtime, payload.clone());
    Ok(JsonBody::ok(payload))
}

/// `tuple(sorted(xs)) if xs else ()`.
///
/// `sorted()` on `str` is code-point order, and Rust's `str` ordering is UTF-8
/// byte order — which agrees with code-point order for every string, so the two
/// key spaces are identical.
fn sorted_or_empty(values: Option<&[String]>) -> Vec<String> {
    let mut out: Vec<String> = values.unwrap_or_default().to_vec();
    out.sort();
    out
}

/// The blocking body: open the store, run the three services, assemble.
fn compute_optimize(
    state: &AppState,
    scope: &crate::services::scope::Scope,
    project: Option<&[String]>,
    exclude: Option<&[String]>,
) -> Result<Value, HttpError> {
    let conn = state
        .connect()
        .map_err(|err| HttpError::new(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?;
    let internal =
        |err: rusqlite::Error| HttpError::new(StatusCode::INTERNAL_SERVER_ERROR, err.to_string());

    // LAW 2 — the engine is the store's primed `price_book` when there is one,
    // never `default_engine()`'s bare manifest.
    let engine = crate::pricing::engine(&conn, state.package_dir())
        .map_err(|err| HttpError::new(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?;

    // `warnings` — the mart-backfill hint, emitted BEFORE any detector runs so
    // the same gate the detectors consult decides the message.
    let mut warnings: Vec<Value> = Vec::new();
    if !mart_queries::mart_has_message_tool_rows(&conn).map_err(internal)? {
        let mut warning = Map::new();
        warning.insert("code".to_owned(), Value::from("mart_empty"));
        warning.insert("level".to_owned(), Value::from("info"));
        warning.insert(
            "message".to_owned(),
            Value::from(
                "message_tool_mart is empty — optimize detectors are \
                 running on the raw messages table and will be slower. \
                 Backfill via the ETL pipeline for the fast path.",
            ),
        );
        warnings.push(Value::Object(warning));
    }

    let waste = optimize::find_waste(&conn, qa_db_path(state).as_deref(), scope, project, exclude)
        .map_err(internal)?;
    let roots = FsRoots::from_env();
    let patterns = optimize::find_patterns(
        &conn,
        &engine,
        &roots,
        Some(scope),
        project,
        &optimize::lookback_iso(30),
    )
    .map_err(internal)?;
    let anomalies = anomaly::find_cost_anomalies(&conn, Some(scope)).map_err(internal)?;

    let pattern_dicts: Vec<Value> = patterns.iter().map(optimize::Finding::to_dict).collect();

    // LAW 3, twice: `sum(...)` is Neumaier-compensated on its float fast path,
    // and `sum([])` is the `int` 0 — which renders `0`, not `0.0`. `round(0, 4)`
    // stays an int too.
    let mut acc = Neumaier::default();
    for pattern in &pattern_dicts {
        // `p.get("estimated_waste_usd") or 0.0` — null AND 0.0 both give 0.0.
        acc.add(
            pattern
                .get("estimated_waste_usd")
                .and_then(Value::as_f64)
                .unwrap_or(0.0),
        );
    }
    let total_waste_usd = match acc.finish_pynum() {
        PyNum::Int(value) => Value::from(value),
        PyNum::Float(value) => serde_json::Number::from_f64(round_half_even(value, 4))
            .map_or(Value::Null, Value::Number),
    };

    let mut payload = Map::new();
    payload.insert("scope".to_owned(), Value::from(scope.label.clone()));
    payload.insert("waste".to_owned(), Value::Array(waste));
    payload.insert("patterns".to_owned(), Value::Array(pattern_dicts));
    payload.insert("total_waste_usd".to_owned(), total_waste_usd);
    payload.insert("anomalies".to_owned(), anomalies);
    payload.insert("warnings".to_owned(), Value::Array(warnings));
    payload.insert("cache".to_owned(), Value::from("miss"));
    Ok(Value::Object(payload))
}

/// `QA_DB_PATH` — `app_dir() / "qa_pairs.db"`, the same derivation
/// `routes/qa.rs` uses.
fn qa_db_path(state: &AppState) -> Option<PathBuf> {
    state
        .store_path()
        .parent()
        .map(|dir| dir.join("qa_pairs.db"))
}

// ── campaign #7 — prescriptions ──────────────────────────────────────────────

/// `GET /api/optimize/prescriptions`.
///
/// Routing recommendations plus CLAUDE.md slim previews, both advisory and both
/// read-only. Dollar fields are "converted" into the active currency — a no-op
/// at `rate == 1.0`, which is the only rate `crate::currency` resolves
/// (DIV-112), so the multiply branch of `_convert_routing` / `_convert_preview`
/// is not written rather than written blind.
async fn get_prescriptions(
    State(state): State<AppState>,
    RawQuery(raw): RawQuery,
) -> HandlerResult {
    let query = Query::parse(raw.as_deref().unwrap_or_default());
    let period = query.str_or("period", "30days").to_owned();
    if !VALID_PERIODS.contains(&period.as_str()) {
        return Err(HttpError::bad_request(format!(
            "Unknown period '{period}'. Valid: {}",
            VALID_PERIODS.join(", ")
        )));
    }
    let scope = parse_period(&period, Instant::now_utc()).map_err(HttpError::bad_request)?;

    // `_slug_for_prescriptions`: the explicit param, else the ACTIVE project's
    // log-dir basename, else `None` (whole store). `project if isinstance(...)`
    // is direct-call tolerance for tests and has no effect over HTTP.
    let explicit = query.get("project").filter(|v| !v.is_empty());
    let slug = match explicit {
        Some(project) => Some(project.to_owned()),
        None => state
            .current_project()
            .log_path
            .filter(|path| !path.is_empty())
            .map(|path| {
                std::path::Path::new(&path)
                    .file_name()
                    .map_or_else(String::new, |name| name.to_string_lossy().into_owned())
            }),
    };

    // The currency stamp is resolved before the worker so a non-USD
    // configuration fails fast rather than after a mart sweep.
    let currency = active_currency_payload(&state.config().currency)
        .map_err(|err| HttpError::new(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?;

    // Captured BEFORE the scope moves into the worker: one clock read, one
    // label. Re-deriving it afterwards would be a second `parse_period`.
    let label = scope.label.clone();
    let worker = state.clone();
    let worker_slug = slug.clone();
    let (routing, previews) = tokio::task::spawn_blocking(move || {
        compute_prescriptions(&worker, &scope, worker_slug.as_deref())
    })
    .await
    .map_err(|err| join_failure(&err))??;

    let mut out = Map::new();
    out.insert("scope".to_owned(), Value::from(label));
    out.insert("project".to_owned(), slug.map_or(Value::Null, Value::from));
    out.insert("routing".to_owned(), routing);
    out.insert("claudemd_previews".to_owned(), Value::Array(previews));
    out.insert("currency".to_owned(), currency);
    Ok(JsonBody::ok(Value::Object(out)))
}

/// The blocking body of `GET /api/optimize/prescriptions`.
fn compute_prescriptions(
    state: &AppState,
    scope: &Scope,
    slug: Option<&str>,
) -> Result<(Value, Vec<Value>), HttpError> {
    let conn = state
        .connect()
        .map_err(|err| HttpError::new(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?;
    let engine = crate::pricing::engine(&conn, state.package_dir())
        .map_err(|err| HttpError::new(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?;

    // `_project_ids_for_slug(conn, slug) if slug else None`. An UNKNOWN slug
    // yields `[]`, which `build_routing_recommendations` treats as "matched
    // nothing" — advisory scope, never the whole store.
    let project_ids = slug.map(|slug| project_ids_for_slug(&conn, slug));
    let candidates = routing_candidates(state.package_dir());
    let routing = build_routing_recommendations(
        &conn,
        &engine,
        &candidates,
        Some(scope),
        project_ids.as_deref(),
    );

    let roots = FsRoots::from_env();
    let filter = slug.map(|slug| vec![slug.to_owned()]);
    let bloat_findings = optimize::find_claudemd_bloat(&engine, &roots, filter.as_deref());

    let mut previews: Vec<Value> = Vec::new();
    for finding in &bloat_findings {
        let finding_dict = finding.to_dict();
        let files = finding
            .details
            .get("files")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        for entry in files {
            // `path = entry.get("path"); if not path: continue`.
            let Some(path) = entry
                .get("path")
                .and_then(Value::as_str)
                .filter(|p| !p.is_empty())
            else {
                continue;
            };
            // `_read_text_defensive` — read-only, and only ever called with a
            // path `find_claudemd_bloat` just produced. `errors="replace"`.
            let Ok(bytes) = std::fs::read(path) else {
                continue;
            };
            let text = String::from_utf8_lossy(&bytes);
            let mut preview = generate_claudemd_preview(
                &engine,
                &text,
                Some(std::slice::from_ref(&finding_dict)),
                path,
                DEFAULT_SESSIONS_PER_MONTH,
            );
            if preview.get("changed") == Some(&Value::Bool(true)) {
                // A NEW key, so it lands after `heuristic` — last.
                if let Value::Object(map) = &mut preview {
                    map.insert("source_path".to_owned(), Value::from(path));
                }
                previews.push(preview);
            }
        }
    }
    Ok((routing, previews))
}

/// `_project_ids_for_slug` — `[]` on an unknown slug or a bad store, never a 500.
fn project_ids_for_slug(conn: &Connection, slug: &str) -> Vec<i64> {
    let Ok(mut stmt) = conn.prepare("SELECT id FROM projects WHERE slug = ?") else {
        return Vec::new();
    };
    stmt.query_map([slug], |row| row.get::<_, i64>(0))
        .map(|rows| rows.flatten().collect())
        .unwrap_or_default()
}

// ── POST /api/optimize/claudemd-preview ──────────────────────────────────────

/// `class ClaudeMdPreviewBody(BaseModel)`, after validation.
#[derive(Debug, Clone)]
struct PreviewBody {
    text: String,
    file_label: String,
    sessions_per_month: i64,
}

/// `POST /api/optimize/claudemd-preview`.
///
/// Exists for CLAUDE.md files outside the locations `/api/optimize` already
/// scans: the client sends the TEXT, never a path, and the server computes the
/// preview purely from the request body — no filesystem read, no write, ever.
async fn post_claudemd_preview(State(state): State<AppState>, body: Bytes) -> HandlerResult {
    let parsed = match parse_preview_body(&body) {
        Ok(parsed) => parsed,
        Err(detail) => {
            return Ok(JsonBody::with_status(
                StatusCode::UNPROCESSABLE_ENTITY,
                detail,
            ));
        }
    };

    // `len(body.text.encode("utf-8", errors="replace")) > MAX_CLAUDEMD_BYTES` —
    // BYTES here, where `approx_tokens` counts code points eight lines later
    // (DIV-117). A `str` that survived JSON decoding has no unencodable code
    // points, so `errors="replace"` never fires and this is `text.len()`.
    if parsed.text.len() > MAX_CLAUDEMD_BYTES {
        return Err(HttpError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            format!("CLAUDE.md text exceeds {MAX_CLAUDEMD_BYTES} bytes"),
        ));
    }

    let currency = active_currency_payload(&state.config().currency)
        .map_err(|err| HttpError::new(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?;

    let worker = state.clone();
    let preview = tokio::task::spawn_blocking(move || -> Result<Value, HttpError> {
        // The pricing engine needs the store's price book (LAW 2), which is the
        // only reason this pure function touches sqlite at all.
        let conn = worker
            .connect()
            .map_err(|err| HttpError::new(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?;
        let engine = crate::pricing::engine(&conn, worker.package_dir())
            .map_err(|err| HttpError::new(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?;
        Ok(generate_claudemd_preview(
            &engine,
            &parsed.text,
            None,
            &parsed.file_label,
            parsed.sessions_per_month,
        ))
    })
    .await
    .map_err(|err| join_failure(&err))??;

    let mut out = Map::new();
    // `_convert_preview(preview, rate)` is the identity at rate 1.0 — DIV-112.
    out.insert("preview".to_owned(), preview);
    out.insert("currency".to_owned(), currency);
    Ok(JsonBody::ok(Value::Object(out)))
}

/// pydantic v2's validation of `ClaudeMdPreviewBody`, error shapes included.
///
/// Every shape below was MEASURED against the reference (FastAPI 0.123.9 /
/// pydantic 2.11.7) rather than inferred, because the `detail` list is a very
/// specific byte shape. In particular:
///
/// * errors are collected for EVERY field and reported in DECLARATION order;
/// * `missing` echoes the whole body object as `input`, a type error echoes the
///   offending value;
/// * the range errors carry a `ctx` object (`{"ge":1}` / `{"le":100000}`) that
///   the type errors do not;
/// * lax mode accepts `"5"` and `" 5 "` and `5.0` for an `int`, and accepts
///   `true`/`false` as `1`/`0` — so `false` fails the `ge=1` bound rather than
///   the type check;
/// * an unknown extra key is IGNORED, not rejected.
///
/// The one shape not reproduced is malformed JSON: pydantic reports
/// `{"type":"json_invalid","loc":["body",<offset>],…,"ctx":{"error":<parser
/// message>}}`, and both the offset and the message come from pydantic-core's
/// own parser. Recorded as DIV — no case row claims it.
fn parse_preview_body(body: &Bytes) -> Result<PreviewBody, Value> {
    if body.is_empty() {
        return Err(detail_list(vec![error_entry(
            "missing",
            vec![Value::from("body")],
            "Field required",
            Value::Null,
            None,
        )]));
    }
    let Ok(parsed) = serde_json::from_slice::<Value>(body) else {
        // NOT best effort any more, and not pydantic-core's parser either: the
        // doc comment's guess was wrong. `fastapi/routing.py` calls
        // `await request.json()` for a `BaseModel` body exactly as it does for a
        // `dict` one, and catches CPython's `JSONDecodeError` before pydantic
        // sees a byte — so the offset and the message are CPython's, and this
        // shape is the one DIV-367 measured and shares with the ten dict-bodied
        // handlers. Hard-coding `0` / `"Expecting value"` was right only for a
        // body that fails at its first character.
        return Err(crate::json::json_invalid_detail(body));
    };
    let Some(obj) = parsed.as_object() else {
        return Err(detail_list(vec![error_entry(
            "model_attributes_type",
            vec![Value::from("body")],
            "Input should be a valid dictionary or object to extract fields from",
            parsed.clone(),
            None,
        )]));
    };

    let mut errors: Vec<Value> = Vec::new();

    // 1. `text: str` — required.
    let text = match obj.get("text") {
        Some(Value::String(text)) => Some(text.clone()),
        None => {
            errors.push(error_entry(
                "missing",
                vec![Value::from("body"), Value::from("text")],
                "Field required",
                parsed.clone(),
                None,
            ));
            None
        }
        Some(other) => {
            errors.push(error_entry(
                "string_type",
                vec![Value::from("body"), Value::from("text")],
                "Input should be a valid string",
                other.clone(),
                None,
            ));
            None
        }
    };

    // 2. `file_label: str = "CLAUDE.md"`.
    let file_label = match obj.get("file_label") {
        Some(Value::String(label)) => Some(label.clone()),
        None => Some("CLAUDE.md".to_owned()),
        Some(other) => {
            errors.push(error_entry(
                "string_type",
                vec![Value::from("body"), Value::from("file_label")],
                "Input should be a valid string",
                other.clone(),
                None,
            ));
            None
        }
    };

    // 3. `sessions_per_month: int = Field(default=…, ge=1, le=100_000)`.
    let loc = || vec![Value::from("body"), Value::from("sessions_per_month")];
    let sessions_per_month = match obj.get("sessions_per_month") {
        None => Some(DEFAULT_SESSIONS_PER_MONTH),
        Some(Value::Bool(flag)) => Some(i64::from(*flag)),
        Some(Value::Number(number)) => {
            if let Some(value) = number.as_i64() {
                Some(value)
            } else if let Some(float) = number.as_f64().filter(|f| f.fract() == 0.0) {
                #[allow(
                    clippy::cast_possible_truncation,
                    reason = "the fract() == 0.0 guard plus the ge/le bounds keep this in range"
                )]
                Some(float as i64)
            } else {
                errors.push(error_entry(
                    "int_from_float",
                    loc(),
                    "Input should be a valid integer, got a number with a fractional part",
                    Value::Number(number.clone()),
                    None,
                ));
                None
            }
        }
        Some(Value::String(raw)) => match raw.trim().parse::<i64>() {
            Ok(value) => Some(value),
            Err(_) => {
                errors.push(error_entry(
                    "int_parsing",
                    loc(),
                    "Input should be a valid integer, unable to parse string as an integer",
                    Value::from(raw.clone()),
                    None,
                ));
                None
            }
        },
        Some(other) => {
            errors.push(error_entry(
                "int_type",
                loc(),
                "Input should be a valid integer",
                other.clone(),
                None,
            ));
            None
        }
    };
    // The bounds run only when the coercion succeeded, and the echoed `input`
    // is the ORIGINAL value (`false`, not `0`).
    let sessions_per_month = match sessions_per_month {
        Some(value) if value < 1 => {
            let mut ctx = Map::new();
            ctx.insert("ge".to_owned(), Value::from(1));
            errors.push(error_entry(
                "greater_than_equal",
                loc(),
                "Input should be greater than or equal to 1",
                obj.get("sessions_per_month")
                    .cloned()
                    .unwrap_or(Value::Null),
                Some(Value::Object(ctx)),
            ));
            None
        }
        Some(value) if value > 100_000 => {
            let mut ctx = Map::new();
            ctx.insert("le".to_owned(), Value::from(100_000));
            errors.push(error_entry(
                "less_than_equal",
                loc(),
                "Input should be less than or equal to 100000",
                obj.get("sessions_per_month")
                    .cloned()
                    .unwrap_or(Value::Null),
                Some(Value::Object(ctx)),
            ));
            None
        }
        other => other,
    };

    if !errors.is_empty() {
        return Err(detail_list(errors));
    }
    Ok(PreviewBody {
        text: text.unwrap_or_default(),
        file_label: file_label.unwrap_or_default(),
        sessions_per_month: sessions_per_month.unwrap_or(DEFAULT_SESSIONS_PER_MONTH),
    })
}

/// One pydantic error object — `type`, `loc`, `msg`, `input`, and `ctx` last.
fn error_entry(kind: &str, loc: Vec<Value>, msg: &str, input: Value, ctx: Option<Value>) -> Value {
    let mut entry = Map::new();
    entry.insert("type".to_owned(), Value::from(kind));
    entry.insert("loc".to_owned(), Value::Array(loc));
    entry.insert("msg".to_owned(), Value::from(msg));
    entry.insert("input".to_owned(), input);
    if let Some(ctx) = ctx {
        entry.insert("ctx".to_owned(), ctx);
    }
    Value::Object(entry)
}

/// `{"detail": [...]}` — FastAPI's `RequestValidationError` handler.
fn detail_list(errors: Vec<Value>) -> Value {
    let mut obj = Map::new();
    obj.insert("detail".to_owned(), Value::Array(errors));
    Value::Object(obj)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(period: &str, project: &[&str], exclude: &[&str]) -> CacheKey {
        (
            period.to_owned(),
            sorted_or_empty(Some(
                &project.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>(),
            )),
            sorted_or_empty(Some(
                &exclude.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>(),
            )),
        )
    }

    #[test]
    fn the_repeated_params_are_sorted_so_the_two_orders_are_one_key() {
        assert_eq!(
            key("30days", &["b", "a"], &[]),
            key("30days", &["a", "b"], &[])
        );
        // …and an absent list is the same key as a present-but-empty one,
        // because `tuple(sorted(project)) if project else ()`.
        assert_eq!(sorted_or_empty(None), sorted_or_empty(Some(&[])));
    }

    #[test]
    fn a_moved_store_mtime_misses_and_leaves_the_stale_entry_in_place() {
        let mut cache = OptimizeCache::default();
        let k = key("30days", &[], &[]);
        cache.put(k.clone(), 100, Value::from("old"));
        assert_eq!(cache.get(&k, 100), Some(Value::from("old")));
        // Same key, newer store: a miss, and the entry is NOT evicted.
        assert_eq!(cache.get(&k, 200), None);
        assert_eq!(cache.entries.len(), 1);
        assert_eq!(cache.get(&k, 100), Some(Value::from("old")));
    }

    #[test]
    fn the_trim_is_fifo_and_runs_before_the_insert_even_for_a_present_key() {
        let mut cache = OptimizeCache::default();
        for i in 0..OPTIMIZE_CACHE_MAX {
            cache.put(key(&format!("p{i}"), &[], &[]), 1, Value::from(i));
        }
        assert_eq!(cache.entries.len(), OPTIMIZE_CACHE_MAX);
        // A brand-new key at capacity evicts the OLDEST INSERTED, not the
        // least recently read.
        assert!(cache.get(&key("p0", &[], &[]), 1).is_some());
        cache.put(key("fresh", &[], &[]), 1, Value::from("f"));
        assert!(cache.get(&key("p0", &[], &[]), 1).is_none());
        assert_eq!(cache.entries.len(), OPTIMIZE_CACHE_MAX);

        // Re-writing a key that is ALREADY present still trims first — Python
        // checks `len(...) >= MAX` before it looks the key up. So the cache
        // shrinks by one and the rewritten key keeps its position.
        cache.put(key("p5", &[], &[]), 1, Value::from("v2"));
        assert_eq!(cache.entries.len(), OPTIMIZE_CACHE_MAX - 1);
        assert_eq!(cache.get(&key("p5", &[], &[]), 1), Some(Value::from("v2")));
        assert!(
            cache.get(&key("p1", &[], &[]), 1).is_none(),
            "p1 was evicted"
        );
    }

    #[test]
    fn evicting_the_key_being_written_re_appends_it_at_the_tail() {
        // The pathological Python case: at capacity, writing the OLDEST key
        // pops it and then re-inserts it, so it becomes the NEWEST.
        let mut cache = OptimizeCache::default();
        for i in 0..OPTIMIZE_CACHE_MAX {
            cache.put(key(&format!("p{i}"), &[], &[]), 1, Value::from(i));
        }
        cache.put(key("p0", &[], &[]), 1, Value::from("again"));
        assert_eq!(cache.entries.len(), OPTIMIZE_CACHE_MAX);
        assert_eq!(cache.entries[0].key, key("p1", &[], &[]));
        assert_eq!(
            cache.entries[OPTIMIZE_CACHE_MAX - 1].key,
            key("p0", &[], &[])
        );
    }

    #[test]
    fn the_period_error_lists_the_valid_set_in_sorted_order() {
        // `sorted({"today","7days","30days","month","all"})` — "30days" first,
        // because '3' sorts before '7' and both before the letters.
        assert_eq!(VALID_PERIODS.join(", "), "30days, 7days, all, month, today");
        assert_eq!(
            HttpError::bad_request(format!(
                "Unknown period '{}'. Valid: {}",
                "week",
                VALID_PERIODS.join(", ")
            ))
            .body()
            .render(),
            r#"{"detail":"Unknown period 'week'. Valid: 30days, 7days, all, month, today"}"#
        );
    }

    #[test]
    fn a_bad_force_flag_is_pydantics_422_not_a_bare_detail_string() {
        let err = Query::parse("force=maybe")
            .bool_or("force", false)
            .expect_err("not a boolean");
        assert_eq!(
            validation_422(&err).render(),
            r#"{"detail":[{"type":"bool_parsing","loc":["query","force"],"msg":"Input should be a valid boolean, unable to interpret input","input":"maybe"}]}"#
        );
    }

    #[test]
    fn a_missing_store_file_has_an_mtime_of_zero_rather_than_an_error() {
        assert_eq!(
            store_mtime_ns(std::path::Path::new("/nonexistent/store.db")),
            0
        );
    }

    /// The rendered 422 body for a body pydantic refuses.
    fn refusal(body: &str) -> String {
        let Err(detail) = parse_preview_body(&Bytes::from(body.to_owned())) else {
            panic!("pydantic refuses this body")
        };
        stax_memory::pyjson::dumps_http(&detail)
    }

    #[test]
    fn a_missing_text_echoes_the_whole_body_as_the_input() {
        // Measured against FastAPI 0.123.9 / pydantic 2.11.7.
        assert_eq!(
            refusal("{}"),
            r#"{"detail":[{"type":"missing","loc":["body","text"],"msg":"Field required","input":{}}]}"#
        );
    }

    #[test]
    fn a_wrongly_typed_field_echoes_that_value_not_the_body() {
        assert_eq!(
            refusal(r#"{"text": 3}"#),
            r#"{"detail":[{"type":"string_type","loc":["body","text"],"msg":"Input should be a valid string","input":3}]}"#
        );
        assert_eq!(
            refusal(r#"{"text": null}"#),
            r#"{"detail":[{"type":"string_type","loc":["body","text"],"msg":"Input should be a valid string","input":null}]}"#
        );
        assert_eq!(
            refusal(r#"{"text": "x", "file_label": 3}"#),
            r#"{"detail":[{"type":"string_type","loc":["body","file_label"],"msg":"Input should be a valid string","input":3}]}"#
        );
    }

    #[test]
    fn the_range_errors_carry_a_ctx_object_and_the_type_errors_do_not() {
        assert_eq!(
            refusal(r#"{"text": "x", "sessions_per_month": 0}"#),
            r#"{"detail":[{"type":"greater_than_equal","loc":["body","sessions_per_month"],"msg":"Input should be greater than or equal to 1","input":0,"ctx":{"ge":1}}]}"#
        );
        assert_eq!(
            refusal(r#"{"text": "x", "sessions_per_month": 100001}"#),
            r#"{"detail":[{"type":"less_than_equal","loc":["body","sessions_per_month"],"msg":"Input should be less than or equal to 100000","input":100001,"ctx":{"le":100000}}]}"#
        );
        assert_eq!(
            refusal(r#"{"text": "x", "sessions_per_month": "abc"}"#),
            r#"{"detail":[{"type":"int_parsing","loc":["body","sessions_per_month"],"msg":"Input should be a valid integer, unable to parse string as an integer","input":"abc"}]}"#
        );
        assert_eq!(
            refusal(r#"{"text": "x", "sessions_per_month": 5.5}"#),
            r#"{"detail":[{"type":"int_from_float","loc":["body","sessions_per_month"],"msg":"Input should be a valid integer, got a number with a fractional part","input":5.5}]}"#
        );
        assert_eq!(
            refusal(r#"{"text": "x", "sessions_per_month": []}"#),
            r#"{"detail":[{"type":"int_type","loc":["body","sessions_per_month"],"msg":"Input should be a valid integer","input":[]}]}"#
        );
    }

    #[test]
    fn every_field_is_reported_and_the_order_is_the_declarations() {
        // Not the order of the keys in the request body — the order the model
        // declares them: text, file_label, sessions_per_month.
        assert_eq!(
            refusal(r#"{"sessions_per_month": "z", "file_label": 2, "text": 1}"#),
            concat!(
                r#"{"detail":[{"type":"string_type","loc":["body","text"],"#,
                r#""msg":"Input should be a valid string","input":1},"#,
                r#"{"type":"string_type","loc":["body","file_label"],"#,
                r#""msg":"Input should be a valid string","input":2},"#,
                r#"{"type":"int_parsing","loc":["body","sessions_per_month"],"#,
                r#""msg":"Input should be a valid integer, unable to parse string as an integer","#,
                r#""input":"z"}]}"#,
            )
        );
    }

    #[test]
    fn an_empty_body_and_a_non_object_body_fail_differently() {
        assert_eq!(
            refusal(""),
            r#"{"detail":[{"type":"missing","loc":["body"],"msg":"Field required","input":null}]}"#
        );
        assert_eq!(
            refusal("[]"),
            r#"{"detail":[{"type":"model_attributes_type","loc":["body"],"msg":"Input should be a valid dictionary or object to extract fields from","input":[]}]}"#
        );
    }

    #[test]
    fn lax_mode_accepts_a_numeric_string_a_whole_float_and_a_bool() {
        let ok = |body: &str| parse_preview_body(&Bytes::from(body.to_owned())).expect("accepted");
        assert_eq!(ok(r#"{"text":"x"}"#).sessions_per_month, 100);
        assert_eq!(
            ok(r#"{"text":"x","sessions_per_month":" 5 "}"#).sessions_per_month,
            5
        );
        assert_eq!(
            ok(r#"{"text":"x","sessions_per_month":5.0}"#).sessions_per_month,
            5
        );
        // `true` coerces to 1, which is inside the bound…
        assert_eq!(
            ok(r#"{"text":"x","sessions_per_month":true}"#).sessions_per_month,
            1
        );
        // …and `false` coerces to 0, which fails `ge=1` — a RANGE error, not a
        // type error, and the echoed input is `false`.
        assert_eq!(
            refusal(r#"{"text":"x","sessions_per_month":false}"#),
            r#"{"detail":[{"type":"greater_than_equal","loc":["body","sessions_per_month"],"msg":"Input should be greater than or equal to 1","input":false,"ctx":{"ge":1}}]}"#
        );
        // An unknown extra key is ignored, not rejected.
        assert_eq!(ok(r#"{"text":"x","nope":1}"#).file_label, "CLAUDE.md");
    }

    #[test]
    fn the_oversize_guard_is_the_413_and_it_counts_bytes() {
        assert_eq!(MAX_CLAUDEMD_BYTES, 2_000_000);
        assert_eq!(
            HttpError::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                format!("CLAUDE.md text exceeds {MAX_CLAUDEMD_BYTES} bytes")
            )
            .body()
            .render(),
            r#"{"detail":"CLAUDE.md text exceeds 2000000 bytes"}"#
        );
        // A two-byte character counts twice against the cap and once against
        // `approx_tokens` — DIV-117, and the reason this is `.len()`.
        assert_eq!("é".len(), 2);
        assert_eq!("é".chars().count(), 1);
    }

    #[test]
    fn an_empty_pattern_list_totals_to_an_int_zero_not_a_float() {
        // LAW 3 / DIV-110: `round(sum([]), 4)` is the `int` 0 and renders `0`.
        let acc = Neumaier::default();
        assert_eq!(acc.finish_pynum(), PyNum::Int(0));
        let empty = match acc.finish_pynum() {
            PyNum::Int(v) => Value::from(v),
            PyNum::Float(v) => Value::from(v),
        };
        assert_eq!(stax_memory::pyjson::dumps_http(&empty), "0");

        // One pattern with a null estimate still switches it to a float.
        let mut acc = Neumaier::default();
        acc.add(0.0);
        let one = match acc.finish_pynum() {
            PyNum::Int(v) => Value::from(v),
            PyNum::Float(v) => serde_json::Number::from_f64(round_half_even(v, 4))
                .map_or(Value::Null, Value::Number),
        };
        assert_eq!(stax_memory::pyjson::dumps_http(&one), "0.0");
    }
}
