//! `routes/webhooks.py` — 3 endpoints, wave 5.
//!
//! | Item | Method | FastAPI path | axum path |
//! |---|---|---|---|
//! | `RS-5-110` | `POST` | `/api/webhooks/github` | `/api/webhooks/github` |
//! | `RS-5-111` | `POST` | `/api/webhooks/gitlab` | `/api/webhooks/gitlab` |
//! | `RS-5-112` | `POST` | `/api/webhooks/ci    ` | `/api/webhooks/ci` |
//!
//! **Status: not ported.** This file is the slot; the endpoint batch that owns
//! `routes/webhooks.py` fills in the handlers and the [`register`] body, and
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
