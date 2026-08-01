//! `routes/worktrees.py` — 2 endpoints, wave 5 (batch E).
//!
//! | Item | Method | FastAPI path | axum path | State |
//! |---|---|---|---|---|
//! | `RS-5-114` | `GET ` | `/api/worktrees          ` | `/api/worktrees`           | ported |
//! | `RS-5-115` | `POST` | `/api/worktrees/attribute` | `/api/worktrees/attribute` | ported |
//!
//! Both were DEFERRED under DIV-145 and are now ported. What DIV-145 said about
//! *measurement* still stands and is the reason `!W-worktrees` stays a
//! known-open row: see below.
//!
//! # `GET /api/worktrees` cannot be byte-pinned, and it is not the port's fault
//!
//! The payload carries `"scanned_at": datetime.now(UTC).isoformat()` —
//! microsecond resolution, stamped per request. Two servers asked a second apart
//! disagree in that field by construction, and there is **no query parameter
//! that suppresses it**: the key is unconditional in
//! `assemble_worktrees_payload`. Measured against the reference on `:8099`, two
//! calls 11 s apart differed in exactly one leaf — `scanned_at` — and agreed on
//! every other one, `worktrees[*]` included. So the row is honestly open, and
//! the finding is that the impossibility is one field wide rather than diffuse.
//! The per-`git`-command determinism table is in `parity/DIV-e-worktrees.md`.
//!
//! # `POST /api/worktrees/attribute` gets no case row, ever
//!
//! It writes `projects.worktree_of` and the DIV-078 ruling does not scale with
//! the size of a write. It is idempotent (`{"updated":3}` then `{"updated":0}`
//! on a store where nothing had been stamped — probed on a private copy of the
//! harness home, never on the shared one), and that is precisely what makes a
//! case row *look* safe: python-then-rust on ONE shared home means the second
//! server is answering a question the first already changed the answer to.
//!
//! # The four details that decide the bytes
//!
//! 1. **`log_path` resolves the same way `routes/forks.py` does** — the query
//!    parameter, else `deps.current_log_path`, else whole-store. All three legs
//!    are Python truthiness, so `?log_path=` (the empty string) falls through to
//!    the current project rather than scoping to `""`.
//! 2. **`scope` is the resolved path or the literal `"store"`** — again
//!    truthiness, so a `""` current project renders `"store"`.
//! 3. **`summary` counts by an explicit verdict → counter map.** An unknown
//!    verdict from the service is simply not tallied; it still contributes its
//!    `cost_usd`.
//! 4. **`attributed_cost_usd` is a `+=` chain seeded with the float `0.0`** —
//!    LAW 3. No `sum()`, therefore no Neumaier compensation, and an empty scan
//!    renders `0.0` rather than an int `0`.

use axum::Router;
use axum::extract::{RawQuery, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use serde_json::{Map, Value};
use stax_etl::stats::aggregator::jf;

use crate::currency::active_currency_payload;
use crate::json::{HandlerResult, HttpError, JsonBody, join_failure};
use crate::qs::Query;
use crate::services::scope::Instant;
use crate::services::worktrees::{
    self, Host, SystemHost, VERDICT_ACTIVE, VERDICT_HAS_UNIQUE_WORK, VERDICT_MERGED_SAFE_TO_PRUNE,
};
use crate::state::AppState;

/// `_VERDICT_COUNTERS` — verdict → the `summary` key it increments.
///
/// Kept explicit so an unknown verdict from the service cannot silently skew the
/// counts; it simply is not tallied. Order is irrelevant here (the summary's key
/// order comes from its own literal), but the mapping is not.
fn verdict_counter(verdict: &str) -> Option<&'static str> {
    match verdict {
        VERDICT_ACTIVE => Some("active"),
        VERDICT_MERGED_SAFE_TO_PRUNE => Some("safe_to_prune"),
        VERDICT_HAS_UNIQUE_WORK => Some("has_unique_work"),
        _ => None,
    }
}

/// Mount this module's endpoints onto `router`.
pub fn register(router: Router<AppState>) -> Router<AppState> {
    router
        .route("/api/worktrees", get(get_worktrees))
        .route("/api/worktrees/attribute", post(post_attribute))
}

// ── GET /api/worktrees ───────────────────────────────────────────────────────

async fn get_worktrees(State(state): State<AppState>, RawQuery(raw): RawQuery) -> HandlerResult {
    let query = Query::parse(raw.as_deref().unwrap_or_default());
    // `log_path_str = log_path if isinstance(log_path, str) else None` — the
    // FastAPI-sentinel guard, which `Query::get` has no equivalent of; then
    // `log_path_str or deps.current_log_path`. Both legs are truthiness, so an
    // EMPTY `?log_path=` falls through to the current project.
    let from_query = query.get("log_path").unwrap_or_default();
    let path: Option<String> = if from_query.is_empty() {
        state
            .current_project()
            .log_path
            .filter(|value| !value.is_empty())
    } else {
        Some(from_query.to_owned())
    };

    // `datetime.now(UTC)` is read in `assemble_worktrees_payload` AFTER the
    // scan, so it is read after the scan here too. The scan is where the seconds
    // go (1.4 s on the harness store), and stamping before it would put
    // `scanned_at` a whole scan-duration in the past — a difference no differ
    // could ever see, since the field cannot match anyway, but the order is the
    // reference's and there is no reason to invent a different one.
    let worker = state.clone();
    let scan_path = path.clone();
    let (worktrees, summary) =
        tokio::task::spawn_blocking(move || scan(&worker, scan_path.as_deref(), &SystemHost))
            .await
            .map_err(|err| join_failure(&err))??;
    let scanned_at = Instant::now_utc().isoformat();

    let currency = active_currency_payload(&state.config().currency)
        .map_err(|err| HttpError::new(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?;
    // `if rate != 1.0:` then a walk over every `cost_usd` and the summary total.
    // DIV-052 makes `active_currency_payload` USD-only, so the branch cannot
    // fire; recorded rather than ported blind, exactly as `routes/cost.rs` and
    // `routes/yield_route.rs` already do.

    let mut payload = Map::new();
    // `project_root if project_root else "store"` — truthiness, and `path` has
    // already had the empty string normalised away above.
    payload.insert(
        "scope".to_owned(),
        Value::String(path.unwrap_or_else(|| "store".to_owned())),
    );
    payload.insert("worktrees".to_owned(), Value::Array(worktrees));
    payload.insert("summary".to_owned(), summary);
    payload.insert("scanned_at".to_owned(), Value::String(scanned_at));
    payload.insert("currency".to_owned(), currency);
    Ok(JsonBody::ok(Value::Object(payload)))
}

/// The blocking body of `assemble_worktrees_payload`: the store read plus the
/// git fan-out, then the summary fold.
///
/// Returns `(worktrees, summary)` rather than the whole payload so the route
/// keeps ownership of the clock stamp and the currency block — the two things
/// that are not a function of the scan.
fn scan(
    state: &AppState,
    project_root: Option<&str>,
    host: &dyn Host,
) -> Result<(Vec<Value>, Value), HttpError> {
    let conn = state
        .connect()
        .map_err(|err| HttpError::new(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?;
    let infos = worktrees::list_worktrees(&conn, project_root, host);
    let worktrees: Vec<Value> = infos.iter().map(worktrees::WorktreeInfo::to_dict).collect();
    Ok((worktrees, summarise(&infos)))
}

/// The `summary` dict, in its literal's key order.
///
/// **LAW 3.** `attributed_cost_usd` is seeded with the float `0.0` and stepped
/// with `+=`; there is no `sum()` in this function, so the accumulation is NOT
/// Neumaier-compensated and an empty scan renders `0.0`, not `0`. Compensating
/// it, or emitting an int zero, would each be a divergence in the other
/// direction.
///
/// The fold reads `wt.get("cost_usd")` off the already-built dicts in Python; it
/// reads the same field off the structs here, which is the same number by
/// construction ([`worktrees::WorktreeInfo::to_dict`] copies it).
fn summarise(infos: &[worktrees::WorktreeInfo]) -> Value {
    let mut safe_to_prune = 0_i64;
    let mut has_unique_work = 0_i64;
    let mut active = 0_i64;
    let mut attributed_cost_usd = 0.0_f64;
    for info in infos {
        match verdict_counter(&info.verdict) {
            Some("safe_to_prune") => safe_to_prune += 1,
            Some("has_unique_work") => has_unique_work += 1,
            Some("active") => active += 1,
            // An unrecognised verdict is not tallied — but its cost still is.
            _ => {}
        }
        // `float(wt.get("cost_usd") or 0.0)` — a plain `+=` chain.
        attributed_cost_usd += info.cost_usd;
    }

    let mut summary = Map::new();
    summary.insert(
        "total".to_owned(),
        Value::from(i64::try_from(infos.len()).unwrap_or(i64::MAX)),
    );
    summary.insert("safe_to_prune".to_owned(), Value::from(safe_to_prune));
    summary.insert("has_unique_work".to_owned(), Value::from(has_unique_work));
    summary.insert("active".to_owned(), Value::from(active));
    summary.insert("attributed_cost_usd".to_owned(), jf(attributed_cost_usd));
    Value::Object(summary)
}

// ── POST /api/worktrees/attribute ────────────────────────────────────────────

/// `post_attribute` → `{"updated": <rows changed>}`.
///
/// Writes ONLY the additive attribution column on `projects` — never git.
/// Idempotent: once every fragment is linked, a re-POST answers `0`. NO CASE
/// ROW, on the DIV-078 ruling.
async fn post_attribute(State(state): State<AppState>) -> HandlerResult {
    let worker = state.clone();
    let updated = tokio::task::spawn_blocking(move || {
        let conn = worker
            .connect()
            .map_err(|err| HttpError::new(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?;
        let updated = worktrees::attribute_fragments(&conn);
        // Python then calls `conn.commit()`, guarded only by the `finally` that
        // closes the connection. The store handle is in autocommit mode in both
        // implementations, so every `UPDATE` has already landed and the commit
        // is a no-op — the comment in the reference says as much.
        Ok::<i64, HttpError>(updated)
    })
    .await
    .map_err(|err| join_failure(&err))??;

    let mut payload = Map::new();
    // `int(updated)` — the count is already an int on both sides.
    payload.insert("updated".to_owned(), Value::from(updated));
    Ok(JsonBody::ok(Value::Object(payload)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::worktrees::WorktreeInfo;
    use crate::state::{Config, CurrentProject};
    use axum::body::Body;
    use axum::http::{Method, Request};
    use rusqlite::Connection;
    use tower::ServiceExt as _;

    /// A scratch `STACKUNDERFLOW_HOME` that cleans itself up.
    struct Scratch(std::path::PathBuf);

    impl Scratch {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "stax-worktrees-{tag}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |delta| delta.as_nanos())
            ));
            std::fs::create_dir_all(&dir).expect("mkdir");
            Self(dir)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).ok();
        }
    }

    /// A store with no session cwds at all, so the scan finds no candidate root
    /// and never spawns `git` — the shape that makes the route testable in
    /// process without touching the machine's repos.
    fn seeded_state(scratch: &Scratch) -> AppState {
        let store = scratch.0.join("store.db");
        let conn = Connection::open(&store).expect("open");
        conn.execute_batch(
            "CREATE TABLE projects (id INTEGER PRIMARY KEY, provider TEXT, slug TEXT,
                                    worktree_of TEXT);
             CREATE TABLE sessions (id INTEGER PRIMARY KEY, project_id INTEGER,
                                    first_ts TEXT, last_ts TEXT);
             CREATE TABLE messages (id INTEGER PRIMARY KEY, session_fk INTEGER,
                                    seq INTEGER, raw_json TEXT);
             INSERT INTO projects (id, provider, slug, worktree_of) VALUES
                 (1, 'claude', '-repo', NULL),
                 (2, 'claude', '-repo--worktrees-w', NULL);",
        )
        .expect("seed");
        drop(conn);
        AppState::new(store, scratch.0.clone(), Config::default())
    }

    async fn call(state: &AppState, method: Method, target: &str) -> (StatusCode, String) {
        let app = register(Router::new()).with_state(state.clone());
        let response = app
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(target)
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 22)
            .await
            .expect("body");
        (status, String::from_utf8(bytes.to_vec()).expect("utf-8"))
    }

    /// Replace the one field no two servers can agree on, so the REST of the
    /// body can be asserted byte for byte.
    fn without_stamp(body: &str) -> String {
        let Some(start) = body.find(r#","scanned_at":""#) else {
            return body.to_owned();
        };
        let tail_from = start + r#","scanned_at":""#.len();
        let Some(end) = body[tail_from..].find('"') else {
            return body.to_owned();
        };
        format!("{}{}", &body[..tail_from], &body[tail_from + end..])
    }

    #[tokio::test]
    async fn an_empty_scan_is_the_store_scope_with_float_zero_totals() {
        let scratch = Scratch::new("empty");
        let state = seeded_state(&scratch);
        let (status, body) = call(&state, Method::GET, "/api/worktrees").await;
        assert_eq!(status, StatusCode::OK);
        // `attributed_cost_usd` is `0.0` and not `0`: the seed is a float
        // literal, not `sum()`'s int start. DIV-057's family, LAW 3.
        assert_eq!(
            without_stamp(&body),
            r#"{"scope":"store","worktrees":[],"summary":{"total":0,"safe_to_prune":0,"has_unique_work":0,"active":0,"attributed_cost_usd":0.0},"scanned_at":"","currency":{"code":"USD","symbol":"$","rate_from_usd":1.0,"warning":null}}"#
        );
    }

    #[tokio::test]
    async fn the_scan_is_always_stamped_with_a_microsecond_utc_instant() {
        let scratch = Scratch::new("stamp");
        let state = seeded_state(&scratch);
        let (_, body) = call(&state, Method::GET, "/api/worktrees").await;
        // The reason `!W-worktrees` can never go green: the key is
        // unconditional and its value is the clock.
        assert!(body.contains(r#","scanned_at":"#));
        assert!(body.contains("+00:00"));
        let (_, second) = call(&state, Method::GET, "/api/worktrees").await;
        assert_eq!(without_stamp(&body), without_stamp(&second));
    }

    #[tokio::test]
    async fn an_explicit_log_path_becomes_the_scope_verbatim() {
        let scratch = Scratch::new("scoped");
        let state = seeded_state(&scratch);
        // `/nonexistent/nope` is not a directory, so the scan short-circuits at
        // the `is_dir` stat and no `git` runs — measured against the reference,
        // which answers exactly this.
        let (status, body) = call(
            &state,
            Method::GET,
            "/api/worktrees?log_path=/nonexistent/nope",
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.starts_with(r#"{"scope":"/nonexistent/nope","worktrees":[],"#));
    }

    #[tokio::test]
    async fn an_empty_log_path_falls_through_to_the_current_project_then_to_store() {
        let scratch = Scratch::new("blank");
        let state = seeded_state(&scratch);
        let (_, body) = call(&state, Method::GET, "/api/worktrees?log_path=").await;
        assert!(body.starts_with(r#"{"scope":"store","#));

        state.set_current_project(CurrentProject {
            project_path: Some("/p".to_owned()),
            log_path: Some("/nonexistent/current".to_owned()),
        });
        let (_, body) = call(&state, Method::GET, "/api/worktrees?log_path=").await;
        assert!(body.starts_with(r#"{"scope":"/nonexistent/current","#));
        // And an explicit value still beats the current project.
        let (_, body) = call(
            &state,
            Method::GET,
            "/api/worktrees?log_path=/nonexistent/x",
        )
        .await;
        assert!(body.starts_with(r#"{"scope":"/nonexistent/x","#));
    }

    #[tokio::test]
    async fn a_repeated_log_path_resolves_to_the_last_occurrence() {
        let scratch = Scratch::new("repeat");
        let state = seeded_state(&scratch);
        // starlette takes the LAST value of a repeated scalar — not the first,
        // and not a 422. Measured against the reference (`scope` came back `b`).
        let (_, body) = call(
            &state,
            Method::GET,
            "/api/worktrees?log_path=/nonexistent/a&log_path=/nonexistent/b",
        )
        .await;
        assert!(body.starts_with(r#"{"scope":"/nonexistent/b","#));
    }

    #[tokio::test]
    async fn an_unknown_query_parameter_is_ignored_not_rejected() {
        let scratch = Scratch::new("unknown");
        let state = seeded_state(&scratch);
        let (status, body) = call(&state, Method::GET, "/api/worktrees?nope=1").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.starts_with(r#"{"scope":"store","#));
    }

    #[tokio::test]
    async fn the_attribute_writer_is_idempotent_and_answers_a_bare_count() {
        let scratch = Scratch::new("attr");
        let state = seeded_state(&scratch);
        let (status, body) = call(&state, Method::POST, "/api/worktrees/attribute").await;
        assert_eq!(status, StatusCode::OK);
        // One worktree-shaped slug in the fixture. Probed against the reference
        // on a private copy of the harness home: `{"updated":3}` then
        // `{"updated":0}` for its three fragment rows.
        assert_eq!(body, r#"{"updated":1}"#);
        let (_, body) = call(&state, Method::POST, "/api/worktrees/attribute").await;
        assert_eq!(body, r#"{"updated":0}"#);
    }

    /// The five green rows in `endpoint-cases-e-worktrees.txt`, driven through
    /// the WHOLE app rather than through `register` alone — the 405 comes from
    /// `lib.rs`'s `method_not_allowed_fallback`, which a bare router does not
    /// have. These are the only rows on this path that can go identical, so
    /// they are the only ones a test can prove ahead of the differ.
    #[tokio::test]
    async fn the_unclaimed_methods_answer_fastapis_405_and_touch_nothing() {
        let scratch = Scratch::new("405");
        let state = seeded_state(&scratch);
        for (method, target) in [
            (Method::POST, "/api/worktrees"),
            (Method::PUT, "/api/worktrees"),
            (Method::DELETE, "/api/worktrees"),
            (Method::GET, "/api/worktrees/attribute"),
            (Method::PUT, "/api/worktrees/attribute"),
        ] {
            let app = crate::app(state.clone());
            let response = app
                .oneshot(
                    Request::builder()
                        .method(method.clone())
                        .uri(target)
                        .body(Body::empty())
                        .expect("request"),
                )
                .await
                .expect("response");
            assert_eq!(
                response.status(),
                StatusCode::METHOD_NOT_ALLOWED,
                "{method} {target}"
            );
            let bytes = axum::body::to_bytes(response.into_body(), 1 << 16)
                .await
                .expect("body");
            assert_eq!(
                String::from_utf8(bytes.to_vec()).expect("utf-8"),
                r#"{"detail":"Method Not Allowed"}"#
            );
        }
        // The writer's handler never ran, so nothing was stamped: the first
        // real POST still has work to do.
        let (_, body) = call(&state, Method::POST, "/api/worktrees/attribute").await;
        assert_eq!(body, r#"{"updated":1}"#);
    }

    #[test]
    fn an_unknown_verdict_is_untallied_but_still_contributes_its_cost() {
        let info = |verdict: &str, cost: f64| WorktreeInfo {
            path: "/w".to_owned(),
            branch: None,
            head: None,
            parent_repo: None,
            parent_slug: None,
            dirty_count: 0,
            unique_commits: 0,
            age_days: None,
            verdict: verdict.to_owned(),
            sessions: 0,
            cost_usd: cost,
            prune_commands: vec![],
            note: None,
        };
        let summary = summarise(&[
            info(VERDICT_ACTIVE, 1.5),
            info("SOMETHING_NEW", 2.5),
            info(VERDICT_MERGED_SAFE_TO_PRUNE, 0.0),
        ]);
        assert_eq!(
            stax_memory::pyjson::dumps_http(&summary),
            r#"{"total":3,"safe_to_prune":1,"has_unique_work":0,"active":1,"attributed_cost_usd":4.0}"#
        );
    }

    #[test]
    fn the_verdict_counter_map_is_the_three_literals_and_nothing_else() {
        assert_eq!(verdict_counter(VERDICT_ACTIVE), Some("active"));
        assert_eq!(
            verdict_counter(VERDICT_MERGED_SAFE_TO_PRUNE),
            Some("safe_to_prune")
        );
        assert_eq!(
            verdict_counter(VERDICT_HAS_UNIQUE_WORK),
            Some("has_unique_work")
        );
        assert_eq!(verdict_counter("active"), None);
        assert_eq!(verdict_counter(""), None);
    }
}
