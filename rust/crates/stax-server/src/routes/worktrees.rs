//! `routes/worktrees.py` — 2 endpoints, wave 5 (batch D). **DEFERRED — DIV-145.**
//!
//! | Item | Method | FastAPI path | axum path | State |
//! |---|---|---|---|---|
//! | `RS-5-114` | `GET ` | `/api/worktrees          ` | `/api/worktrees`           | **open** — DIV-145 |
//! | `RS-5-115` | `POST` | `/api/worktrees/attribute` | `/api/worktrees/attribute` | **open** — DIV-145 |
//!
//! `services/worktrees.py` is **753 lines** and it does not only read the store:
//! it shells out to `git` (worktree enumeration, merge-base checks, branch
//! ancestry) for every root the store knows about. So the response is a function
//! of the maintainer's working tree at the instant of the request — two servers
//! asked a second apart can legitimately disagree, and a differ cannot pin it.
//! The payload also stamps `scanned_at = datetime.now(UTC)`.
//!
//! `POST /api/worktrees/attribute` writes: it fills the attribution column on
//! `projects` and commits. Idempotent, and it only ever touches an additive
//! column — but it is still a store write on the shared harness home, and the
//! DIV-078 ruling stands regardless of how small the write is.
//!
//! The whole-store scan is also the one endpoint here that got a performance fix
//! in this repo's own recent history (`98e7f8b`, the cwd scan that parsed every
//! blob three times), which is a fair signal about how much behaviour hides
//! behind "a thin wrapper".
//!
//! Sidecar: `!W-worktrees` only — a read, safe to execute. The attribute writer
//! gets no row, on the DIV-078 ruling.

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
