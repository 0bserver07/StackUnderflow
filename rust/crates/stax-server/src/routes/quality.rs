//! `routes/quality.py` — 2 endpoints, wave 5 (batch E). **Closes DIV-135.**
//!
//! | Item | Method | FastAPI path | axum path | State |
//! |---|---|---|---|---|
//! | `RS-5-102` | `GET ` | `/api/static-analysis/session/{session_id}/quality` | same | ported |
//! | `RS-5-103` | `POST` | `/api/static-analysis/session/{session_id}/grade  ` | same | ported |
//!
//! # What changed since the stub, and what did not
//!
//! The stub's doc comment listed four separately-disqualifying properties of a
//! `GET` that grades a session: a network call, a sampled body, a wall clock,
//! and a store write whose output feeds the next request. Three of the four are
//! answered *structurally* by the fallback path in
//! [`crate::services::grading`], and the module docs there carry the argument
//! with the Python line numbers. The short version: `:11434` refused, so
//! `is_fallback` is `True`, the body is a frozen literal, and grading.py:205
//! skips the `INSERT OR REPLACE` — no write, no commit, idempotent.
//!
//! The fourth is not answered. `graded_at` is `datetime.now(UTC)` and it is *in
//! the body*, so `!QL-quality-real` stays known-open with that as its whole
//! reason. `parity/DIV-e-quality.md` records an isolated two-server probe that
//! diffed every other field and found them identical, which is the evidence the
//! case row cannot itself produce.
//!
//! # The two legs that ARE fully deterministic
//!
//! * **The 404.** `SELECT id FROM sessions WHERE session_id = ?` misses, and
//!   `HTTPException(404, f"Session {session_id} not found")` fires *before* the
//!   grader on both endpoints. `!QL-quality-missing` / `!QL-grade-missing` flip
//!   to green on this. The detail string is an f-string with the raw
//!   `session_id`, so a percent-encoded id round-trips through starlette's
//!   decode and lands in the body as UTF-8 (`ensure_ascii=False` — law 1); the
//!   `QL-*-unicode` rows pin exactly that.
//! * **A stored grade.** [`crate::services::grading::get_stored_grade`] is one
//!   indexed `SELECT`, no clock and no socket. It has no case row because
//!   `session_quality_metrics` holds **0 rows** in `.parity-state/fresh` — only
//!   real LLM grades are ever persisted and nothing has graded that snapshot.
//!   Measured, not assumed; see the DIV note.
//!
//! # `schema.apply` is not called here, so no table guard is ported
//!
//! `routes/static_analysis.py` migrates on every GET, which is why its port
//! needed a table-existence stand-in (DIV-134). `routes/quality.py` does not,
//! and neither does `runner.get_session_quality`. So a missing
//! `static_analysis_findings` / `messages` object is an `OperationalError` and a
//! 500 on *both* sides, and adding the neighbour's guard here would be the
//! divergence. Law 7 in reverse.

use axum::Router;
use axum::extract::{Path as PathParam, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use rusqlite::Connection;
use serde_json::Value;

use crate::json::{HandlerResult, HttpError, JsonBody, join_failure};
use crate::services::grading::{DEFAULT_OLLAMA_URL, GradeError, get_stored_grade, grade_session};
use crate::state::AppState;

/// Mount this module's endpoints onto `router`.
pub fn register(router: Router<AppState>) -> Router<AppState> {
    router
        .route(
            "/api/static-analysis/session/{session_id}/quality",
            get(get_quality),
        )
        .route(
            "/api/static-analysis/session/{session_id}/grade",
            post(post_grade),
        )
}

// ── GET /api/static-analysis/session/{session_id}/quality ────────────────────

/// `get_quality` — "retrieve session quality metrics, performing lazy grading
/// if missing".
async fn get_quality(
    State(state): State<AppState>,
    PathParam(session_id): PathParam<String>,
) -> HandlerResult {
    run(state, session_id, false).await
}

// ── POST /api/static-analysis/session/{session_id}/grade ─────────────────────

/// `post_grade` — the same thing with `force=True`, i.e. unconditionally.
///
/// It takes no request body. FastAPI declares no body parameter, so anything
/// sent is ignored and never parsed; the differ's `-` (no body) and a stray JSON
/// object are the same request as far as either server is concerned.
async fn post_grade(
    State(state): State<AppState>,
    PathParam(session_id): PathParam<String>,
) -> HandlerResult {
    run(state, session_id, true).await
}

/// The shared body of both handlers — they differ only in `force`.
///
/// Python opens the connection in the handler and closes it in a `finally`,
/// which is what dropping the `Connection` at the end of the blocking task
/// does. The session check comes FIRST in both, deliberately: it is the leg
/// that has to answer before anything can reach the grader.
async fn run(state: AppState, session_id: String, force: bool) -> HandlerResult {
    let worker = state.clone();
    let grade = tokio::task::spawn_blocking(move || -> Result<Value, HttpError> {
        let conn = worker.connect().map_err(|err| any_500(&err))?;
        if !session_exists(&conn, &session_id).map_err(sql_500)? {
            // The f-string, byte for byte. `ensure_ascii=False` means a non-ASCII
            // session id ships as raw UTF-8 in the detail.
            return Err(HttpError::not_found(format!(
                "Session {session_id} not found"
            )));
        }
        if !force {
            // `grade = get_stored_grade(...)`; only a miss reaches the grader.
            if let Some(stored) = get_stored_grade(&conn, &session_id).map_err(grade_500)? {
                return Ok(stored);
            }
        }
        grade_session(&conn, &session_id, force, DEFAULT_OLLAMA_URL).map_err(grade_500)
    })
    .await
    .map_err(|err| join_failure(&err))??;
    Ok(JsonBody::ok(grade))
}

/// `conn.execute("SELECT id FROM sessions WHERE session_id = ?").fetchone()`
/// — the value is never read, only its presence.
fn session_exists(conn: &Connection, session_id: &str) -> rusqlite::Result<bool> {
    let mut stmt = conn.prepare("SELECT id FROM sessions WHERE session_id = ?")?;
    let mut rows = stmt.query([session_id])?;
    Ok(rows.next()?.is_some())
}

fn sql_500(err: rusqlite::Error) -> HttpError {
    HttpError::new(StatusCode::INTERNAL_SERVER_ERROR, err.to_string())
}

fn grade_500(err: GradeError) -> HttpError {
    HttpError::new(StatusCode::INTERNAL_SERVER_ERROR, err.to_string())
}

fn any_500(err: &anyhow::Error) -> HttpError {
    HttpError::new(StatusCode::INTERNAL_SERVER_ERROR, err.to_string())
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt as _;

    use super::*;

    /// A store with one session, one message and no stored grade — the shape
    /// `.parity-state/fresh` has for every session in it.
    fn fixture(path: &std::path::Path) {
        let conn = Connection::open(path).expect("open");
        conn.execute_batch(
            "CREATE TABLE sessions (id INTEGER PRIMARY KEY, session_id TEXT NOT NULL UNIQUE);
             CREATE TABLE messages (
                 id INTEGER PRIMARY KEY, session_fk INTEGER, seq INTEGER,
                 role TEXT, content_text TEXT);
             CREATE TABLE static_analysis_findings (
                 session_id TEXT, file_path TEXT, language TEXT, ts TEXT, metric TEXT,
                 pre_value REAL, post_value REAL, delta REAL, details_json TEXT);
             CREATE TABLE session_quality_metrics (
                 id INTEGER PRIMARY KEY, session_id TEXT NOT NULL UNIQUE,
                 overall_score REAL NOT NULL, grades_json TEXT NOT NULL,
                 rationale TEXT NOT NULL, suggestions_json TEXT NOT NULL,
                 graded_at TEXT NOT NULL);
             INSERT INTO sessions (id, session_id) VALUES (1, 'real-session');
             INSERT INTO messages (session_fk, seq, role, content_text)
                 VALUES (1, 1, 'user', 'hello');",
        )
        .expect("schema");
    }

    struct Fixture {
        _dir: std::path::PathBuf,
        state: AppState,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self._dir).ok();
        }
    }

    fn state() -> Fixture {
        let dir = std::env::temp_dir().join(format!(
            "stax-quality-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).expect("tmpdir");
        let store = dir.join("store.db");
        fixture(&store);
        let state = AppState::new(store, dir.join("pkg"), crate::Config::default());
        Fixture { _dir: dir, state }
    }

    async fn call(state: &AppState, method: &str, path: &str) -> (StatusCode, String) {
        let app = crate::app(state.clone());
        let response = app
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(path)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap();
        (status, String::from_utf8(bytes.to_vec()).unwrap())
    }

    /// `!QL-quality-missing` / `!QL-grade-missing` — the rows this batch flips.
    #[tokio::test]
    async fn an_unknown_session_is_the_f_string_404_on_both_endpoints() {
        let fixture = state();
        for (method, path) in [
            (
                "GET",
                "/api/static-analysis/session/no-such-session-anywhere/quality",
            ),
            (
                "POST",
                "/api/static-analysis/session/no-such-session-anywhere/grade",
            ),
        ] {
            let (status, body) = call(&fixture.state, method, path).await;
            assert_eq!(status, StatusCode::NOT_FOUND);
            assert_eq!(
                body,
                r#"{"detail":"Session no-such-session-anywhere not found"}"#
            );
        }
    }

    /// A percent-encoded id is decoded before the f-string, and the body carries
    /// the raw UTF-8 — `ensure_ascii=False`, not `é`.
    #[tokio::test]
    async fn a_non_ascii_session_id_lands_in_the_detail_unescaped() {
        let fixture = state();
        let (status, body) = call(
            &fixture.state,
            "GET",
            "/api/static-analysis/session/caf%C3%A9%20session/quality",
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body, r#"{"detail":"Session café session not found"}"#);
    }

    /// The unclaimed methods. FastAPI's 405 handler, not starlette's plain text.
    #[tokio::test]
    async fn the_unclaimed_method_on_each_path_is_the_json_405() {
        let fixture = state();
        for (method, path) in [
            ("POST", "/api/static-analysis/session/x/quality"),
            ("GET", "/api/static-analysis/session/x/grade"),
            ("DELETE", "/api/static-analysis/session/x/quality"),
        ] {
            let (status, body) = call(&fixture.state, method, path).await;
            assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED, "{method} {path}");
            assert_eq!(body, r#"{"detail":"Method Not Allowed"}"#);
        }
    }

    /// A real session with no stored grade takes the fallback path, and the only
    /// field that is not a literal is `graded_at`.
    #[tokio::test]
    async fn a_real_session_answers_the_fallback_body_and_writes_nothing() {
        let fixture = state();
        for (method, path) in [
            ("GET", "/api/static-analysis/session/real-session/quality"),
            ("POST", "/api/static-analysis/session/real-session/grade"),
        ] {
            let (status, body) = call(&fixture.state, method, path).await;
            assert_eq!(status, StatusCode::OK);
            let parsed: Value = serde_json::from_str(&body).unwrap();
            assert_eq!(parsed["grade_source"], Value::from("fallback"));
            assert_eq!(parsed["overall_score"], Value::from(5.0));
            assert!(parsed["graded_at"].as_str().unwrap().ends_with('Z'));
            // The prefix up to `graded_at` is byte-stable, which is what the
            // two-server probe in DIV-e-quality.md measures across processes.
            assert!(body.starts_with(
                r#"{"session_id":"real-session","overall_score":5.0,"grades":{"goal_clarity":5.0,"execution_efficiency":5.0,"success":5.0},"rationale":"Fallback grade: local Ollama instance was offline or failed to grade.","suggestions":["Ensure local Ollama service is running on port 11434."],"graded_at":"#
            ));
        }

        // The whole safety argument for a case row on a real session: no row was
        // written, so the second server sees exactly what the first one did.
        let conn = Connection::open(fixture.state.store_path()).unwrap();
        let stored: i64 = conn
            .query_row("SELECT COUNT(*) FROM session_quality_metrics", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(stored, 0);
    }

    /// A stored row short-circuits the GET and is served verbatim — no clock, no
    /// socket, byte-stable. The path that has no case row only because the
    /// harness store has no such row.
    #[tokio::test]
    async fn a_stored_grade_short_circuits_the_get_entirely() {
        let fixture = state();
        let conn = Connection::open(fixture.state.store_path()).unwrap();
        conn.execute(
            "INSERT INTO session_quality_metrics \
             (session_id, overall_score, grades_json, rationale, suggestions_json, graded_at) \
             VALUES ('real-session', 8.25, '{\"goal_clarity\": 9.0}', 'good run', \
                     '[\"ship it\"]', '2026-07-31T00:00:00Z')",
            [],
        )
        .unwrap();
        drop(conn);

        let (status, body) = call(
            &fixture.state,
            "GET",
            "/api/static-analysis/session/real-session/quality",
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body,
            concat!(
                r#"{"session_id":"real-session","overall_score":8.25,"#,
                r#""grades":{"goal_clarity":9.0},"rationale":"good run","#,
                r#""suggestions":["ship it"],"graded_at":"2026-07-31T00:00:00Z","#,
                r#""grade_source":"llm"}"#
            )
        );

        // `force=True` does NOT short-circuit: it re-grades, hits the closed
        // port, and answers the fallback — which is why the POST is the one that
        // must never be pointed at a host with Ollama up.
        let (status, body) = call(
            &fixture.state,
            "POST",
            "/api/static-analysis/session/real-session/grade",
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains(r#""grade_source":"fallback""#));
    }
}
