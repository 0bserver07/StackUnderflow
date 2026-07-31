//! `routes/search.py` — 3 endpoints, wave 6.
//!
//! | Item | Method | FastAPI path | axum path |
//! |---|---|---|---|
//! | `RS-6-016` | `GET ` | `/api/search        ` | `/api/search` |
//! | `RS-6-017` | `POST` | `/api/search/reindex` | `/api/search/reindex` |
//! | `RS-6-018` | `GET ` | `/api/search/stats  ` | `/api/search/stats` |
//!
//! **Status: not ported.** This file is the slot; the endpoint batch that owns
//! `routes/search.py` fills in the handlers and the [`register`] body, and
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
