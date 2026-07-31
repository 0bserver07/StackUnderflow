//! `routes/playback.py` — 3 endpoints, wave 5 (batch D). **DEFERRED — DIV-140.**
//!
//! | Item | Method | FastAPI path | axum path | State |
//! |---|---|---|---|---|
//! | `RS-5-091` | `GET` | `/api/playback/{session_id}/fs       ` | same | **open** — DIV-140 |
//! | `RS-5-092` | `GET` | `/api/playback/{session_id}          ` | same | **open** — DIV-140 |
//! | `RS-5-093` | `GET` | `/api/playback/project/{project_slug}` | same | **open** — DIV-140 |
//!
//! Three thin routes over **1,678 lines** of unported service:
//! `services/playback.py` (882 — the tool-call event extractor over
//! `messages.raw_json`, plus the optional `captured_events` success flag),
//! `services/playback_fs.py` (617 — the v2 virtual-filesystem replay that
//! reconstructs file contents by applying Read/Write/Edit/MultiEdit/NotebookEdit
//! in order), and `services/risk.py` (179 — the per-file revert/failure overlay
//! the `/fs` endpoint decorates each file with).
//!
//! That is a service-layer port three times over, and none of it is shared with
//! anything else batch D touches. The route layer above it is genuinely thin:
//! a comma-split filter, a `Nd|Nh|Nm`-or-ISO `since` parser, and three
//! `JSONResponse` wrappers. Porting the thin part alone would answer nothing.
//!
//! Read-only throughout — `schema.apply` aside — so the sidecar carries safe
//! `!` rows for all three, including the 404 legs, which is what makes the gap
//! measurable rather than merely stated.

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
