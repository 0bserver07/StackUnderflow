//! `routes/forks.py` — 1 endpoint, wave 5 (batch D). **DEFERRED — DIV-142.**
//!
//! | Item | Method | FastAPI path | axum path | State |
//! |---|---|---|---|---|
//! | `RS-5-074` | `GET` | `/api/forks` | `/api/forks` | **open** — DIV-142 |
//!
//! A thin route over `reports/forks.py::analyze_forks` — **534 lines** that load
//! every scoped `messages` row (41K+ on a real project), walk the conversation
//! DAG by `parent_uuid`, price the sidechain share, and infer abandoned
//! branches. The route's own comment measures it at ~6 s warm with no mart to
//! lean on, which is why it carries a process-wide read-through memo keyed on a
//! sessions signature.
//!
//! Two things beyond the line count kept it out of this batch:
//!
//! * the DAG walk is the kind of code where a paraphrase passes a smoke test and
//!   fails on the tenth project, and there is no mart to check it against;
//! * the memo is *process-wide* and its currency conversion is deliberately
//!   outside it, so a port has to reproduce a cache boundary as well as a
//!   computation.
//!
//! The `400` on an unknown `?period=` is route-level and deterministic, but a
//! module that answers only its validation error is the shape DIV-082 ruled
//! against. Read-only, so both sidecar rows are safe to execute.

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
