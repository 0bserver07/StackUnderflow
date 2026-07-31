//! `routes/etl.py` — 2 endpoints, wave 5 (batch D). **DEFERRED — DIV-139.**
//!
//! | Item | Method | FastAPI path | axum path | State |
//! |---|---|---|---|---|
//! | `RS-5-071` | `GET ` | `/api/etl/status  ` | `/api/etl/status`   | **open** — DIV-139 |
//! | `RS-5-072` | `POST` | `/api/etl/backfill` | `/api/etl/backfill` | **open** — DIV-139 |
//!
//! Both halves are out of an endpoint batch's reach, for different reasons.
//!
//! **`GET /api/etl/status` is a thin shell over a 566-line assembler.**
//! `etl/status.py::assemble_status` is where all the work lives — watcher
//! introspection, per-mart watermarks and row counts, `usage_events` rollups by
//! provider and by `cost_source`, a coverage scan for projects with no mart row,
//! a lag computation, and a four-state health verdict. Porting the route means
//! porting the assembler, which is a service-layer item, not an endpoint one.
//! The route itself is four lines.
//!
//! **`POST /api/etl/backfill` is a writer with a process-local lock.** It takes
//! a `threading.Lock`-guarded job slot, schedules `etl/backfill.py::backfill`
//! (365 lines) on FastAPI's `BackgroundTasks`, and returns `202` with a job id
//! and a wall-clock `started_at`. It rebuilds marts on the SHARED harness home,
//! which is the DIV-078 hazard exactly — the run that rebuilt a 520 MB search
//! index taught the batch canon that a `!` row suppresses the verdict, never the
//! request. Its `409` leg needs a job already in flight, which is state a differ
//! cannot arrange for both servers at once.
//!
//! Neither endpoint is a case row: `!Y-etl-status` would be safe but is a pure
//! read of an unported surface, and the backfill row would not be safe at all.
//! `!E-etl-status` is in the sidecar; the backfill is tracked here and in the
//! ledger only, alongside the reindex endpoints that took the same ruling.

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
