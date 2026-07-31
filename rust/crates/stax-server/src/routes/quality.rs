//! `routes/quality.py` — 2 endpoints, wave 5 (batch D). **DEFERRED — DIV-135.**
//!
//! | Item | Method | FastAPI path | axum path | State |
//! |---|---|---|---|---|
//! | `RS-5-102` | `GET ` | `/api/static-analysis/session/{session_id}/quality` | same | **open** — DIV-135 |
//! | `RS-5-103` | `POST` | `/api/static-analysis/session/{session_id}/grade  ` | same | **open** — DIV-135 |
//!
//! # A `GET` that calls an LLM and writes the store
//!
//! This is the sharpest DIV-059 case in the wave, and it is worth stating
//! plainly because the path looks like `routes/static_analysis.py`'s — one
//! segment deeper — and that one ported cleanly.
//!
//! `get_quality` reads `session_quality_metrics`, and **when there is no stored
//! row it grades the session**: `services/grading.py::grade_session` pulls the
//! whole transcript, calls `http://localhost:11434/api/tags` to discover a
//! model, posts the transcript to `/api/chat` with a 30 s timeout, and — if the
//! model answered — `INSERT OR REPLACE`s the result into the store and commits.
//! So a plain `GET` is a network call, a nondeterministic body (a sampled LLM
//! grade, plus a `graded_at` wall clock), and a writer whose output becomes the
//! next request's input on the shared home.
//!
//! Every one of those is separately disqualifying. `POST …/grade` is the same
//! thing with `force=True`, i.e. unconditionally.
//!
//! The one deterministic leg is the 404 (`SELECT id FROM sessions` misses), and
//! it comes before the grading — but a module that answers only its 404 is the
//! shape DIV-082 already ruled against. Unmounted; `!Q-quality-missing` and
//! `!Q-grade-missing` report the gap on the **unknown-session** id only, so
//! neither row can reach the grader on either side.

use axum::Router;

use crate::state::AppState;

/// Mount this module's endpoints onto `router`.
///
/// Returns the router unchanged: the module is DEFERRED, so every path above
/// 404s. A dark surface the ledger names beats a half-lit one nobody can
/// reason about — the ruling `!A-*` / DIV-082 set for `routes/agent_teams.py`.
pub fn register(router: Router<AppState>) -> Router<AppState> {
    router
}
