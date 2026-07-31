//! `routes/qa.py` — 4 endpoints, wave 6.
//!
//! | Item | Method | FastAPI path | axum path |
//! |---|---|---|---|
//! | `RS-6-012` | `GET ` | `/api/qa        ` | `/api/qa` |
//! | `RS-6-013` | `GET ` | `/api/qa/stats  ` | `/api/qa/stats` |
//! | `RS-6-014` | `GET ` | `/api/qa/{qa_id}` | `/api/qa/{qa_id}` |
//! | `RS-6-015` | `POST` | `/api/qa/reindex` | `/api/qa/reindex` |
//!
//! **Status: not ported.** This file is the slot; the endpoint batch that owns
//! `routes/qa.py` fills in the handlers and the [`register`] body, and
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
