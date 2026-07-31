//! `routes/benchmark.py` — 2 endpoints, wave 5 (batch D). **DEFERRED — DIV-143.**
//!
//! | Item | Method | FastAPI path | axum path | State |
//! |---|---|---|---|---|
//! | `RS-5-045` | `GET` | `/api/benchmark          ` | `/api/benchmark`           | **open** — DIV-143 |
//! | `RS-5-046` | `GET` | `/api/benchmark/recommend` | `/api/benchmark/recommend` | **open** — DIV-143 |
//!
//! Two thin routes over `reports/benchmark.py` — **1,033 lines**, the largest
//! unported report in the tree. It re-derives a task intent per session, strata
//! the history by intent/size/language, and computes per-model verdicts with
//! confidence intervals and an explicit "insufficient evidence" outcome. The
//! statistical machinery is the deliverable, and a CI that is subtly wrong is
//! worse than one that is absent: it would read as a verified verdict.
//!
//! The route layer is `routes/forks.py`'s, twice — the same period-alias table,
//! the same signature-keyed memo, the same convert-a-deep-copy currency split —
//! so it ports in an afternoon *after* the report does, and not before.
//!
//! Read-only. Four sidecar `!` rows: both endpoints, plus the two `400` legs
//! (unknown `period`, missing `intent`), so the gap is measured on the error
//! shapes too rather than only on the happy path.

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
