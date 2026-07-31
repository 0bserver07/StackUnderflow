//! `routes/tags.py` — 6 endpoints, wave 6.
//!
//! | Item | Method | FastAPI path | axum path |
//! |---|---|---|---|
//! | `RS-6-019` | `GET   ` | `/api/tags                           ` | `/api/tags` |
//! | `RS-6-020` | `GET   ` | `/api/tags/session/{session_id}      ` | `/api/tags/session/{session_id}` |
//! | `RS-6-021` | `POST  ` | `/api/tags/session/{session_id}      ` | `/api/tags/session/{session_id}` |
//! | `RS-6-022` | `DELETE` | `/api/tags/session/{session_id}/{tag}` | `/api/tags/session/{session_id}/{tag}` |
//! | `RS-6-023` | `GET   ` | `/api/tags/browse/{tag}              ` | `/api/tags/browse/{tag}` |
//! | `RS-6-024` | `POST  ` | `/api/tags/reindex                   ` | `/api/tags/reindex` |
//!
//! **Status: not ported.** This file is the slot; the endpoint batch that owns
//! `routes/tags.py` fills in the handlers and the [`register`] body, and
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
