//! `routes/misc.py` — 6 endpoints, wave 5.
//!
//! | Item | Method | FastAPI path | axum path |
//! |---|---|---|---|
//! | `RS-5-079` | `GET                ` | `/api/pricing            ` | `/api/pricing` |
//! | `RS-5-080` | `POST               ` | `/api/pricing/refresh    ` | `/api/pricing/refresh` |
//! | `RS-5-081` | `GET                ` | `/api/health             ` | `/api/health` |
//! | `RS-5-082` | `GET                ` | `/favicon.ico            ` | `/favicon.ico` |
//! | `RS-5-083` | `GET                ` | `/assets/{full_path:path}` | `/assets/{*full_path}` |
//! | `RS-5-084` | `GET|POST|PUT|DELETE` | `/ollama-api/{path:path} ` | `/ollama-api/{*path}` |
//!
//! **Status: not ported.** This file is the slot; the endpoint batch that owns
//! `routes/misc.py` fills in the handlers and the [`register`] body, and
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
