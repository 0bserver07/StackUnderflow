//! `routes/cost.py` — 4 endpoints, wave 5.
//!
//! | Item | Method | FastAPI path | axum path |
//! |---|---|---|---|
//! | `RS-5-062` | `GET` | `/api/cost-data                   ` | `/api/cost-data` |
//! | `RS-5-063` | `GET` | `/api/cost-data/by-provider       ` | `/api/cost-data/by-provider` |
//! | `RS-5-064` | `GET` | `/api/interaction/{interaction_id}` | `/api/interaction/{interaction_id}` |
//! | `RS-5-065` | `GET` | `/api/cost-data/by-model          ` | `/api/cost-data/by-model` |
//!
//! **Status: not ported.** This file is the slot; the endpoint batch that owns
//! `routes/cost.py` fills in the handlers and the [`register`] body, and
//! touches nothing else in the crate. `routes/mod.rs` already names this module
//! in `server.py`'s `include_router` position, so mounting is not a merge point.

use axum::Router;

use crate::state::AppState;

/// Mount this module's endpoints onto `router`.
///
/// Called once, from [`super::register_all`], at this module's
/// `include_router` position. Returns the router unchanged while the module is
/// unported — an unmounted path 404s, which is what a dark surface should do.
pub fn register(router: Router<AppState>) -> Router<AppState> {
    router
}
