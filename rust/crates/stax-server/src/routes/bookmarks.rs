//! `routes/bookmarks.py` — 6 endpoints, wave 6.
//!
//! | Item | Method | FastAPI path | axum path |
//! |---|---|---|---|
//! | `RS-6-006` | `GET   ` | `/api/bookmarks                     ` | `/api/bookmarks` |
//! | `RS-6-007` | `POST  ` | `/api/bookmarks                     ` | `/api/bookmarks` |
//! | `RS-6-008` | `DELETE` | `/api/bookmarks/{bookmark_id}       ` | `/api/bookmarks/{bookmark_id}` |
//! | `RS-6-009` | `PUT   ` | `/api/bookmarks/{bookmark_id}       ` | `/api/bookmarks/{bookmark_id}` |
//! | `RS-6-010` | `GET   ` | `/api/bookmarks/session/{session_id}` | `/api/bookmarks/session/{session_id}` |
//! | `RS-6-011` | `POST  ` | `/api/bookmarks/toggle              ` | `/api/bookmarks/toggle` |
//!
//! **Status: not ported.** This file is the slot; the endpoint batch that owns
//! `routes/bookmarks.py` fills in the handlers and the [`register`] body, and
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
