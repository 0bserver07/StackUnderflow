//! `routes/patterns.py` — 2 endpoints, wave 5 (batch D). **DEFERRED — DIV-144.**
//!
//! | Item | Method | FastAPI path | axum path | State |
//! |---|---|---|---|---|
//! | `RS-5-088` | `GET ` | `/api/patterns        ` | `/api/patterns`         | **open** — DIV-144 |
//! | `RS-5-089` | `POST` | `/api/patterns/dismiss` | `/api/patterns/dismiss` | **open** — DIV-144 |
//!
//! `GET` is a thin route over `reports/patterns.py::mine_patterns` — **1,097
//! lines** of recurrence mining: per-file failure rates, error-signature
//! clustering with resolution hints, and Bash command failure clusters, all
//! window-bounded and deterministically ordered. A service-layer port.
//!
//! `POST /api/patterns/dismiss` is small but it is a **writer outside the
//! store**: it bumps a dismissal counter in
//! `~/.stackunderflow/proactive_state.json` through `hooks/proactive.py`, whose
//! `make_signal` fingerprint must be byte-identical to the one the Tier-1 hook
//! computes or the dismissal silences nothing. That is a hooks-package contract,
//! not an endpoint one, and the file it writes is outside
//! `$STACKUNDERFLOW_HOME` — so it is not even contained by the harness's shared
//! home the way `config.json` and `tags.json` are.
//!
//! Sidecar rows: `!P-patterns` and `!P-patterns-bad-since` (a read and a `400`).
//! The dismiss endpoint gets **no row** — it would write a governance file the
//! harness does not own, on the maintainer's real machine, twice per run.

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
