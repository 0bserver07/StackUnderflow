//! `routes/cfg.py` — 6 endpoints, wave 5.
//!
//! | Item | Method | FastAPI path | axum path |
//! |---|---|---|---|
//! | `RS-5-050` | `GET   ` | `/api/cfg              ` | `/api/cfg` |
//! | `RS-5-051` | `GET   ` | `/api/cfg/currencies   ` | `/api/cfg/currencies` |
//! | `RS-5-052` | `POST  ` | `/api/cfg/currency     ` | `/api/cfg/currency` |
//! | `RS-5-053` | `GET   ` | `/api/cfg/model-aliases` | `/api/cfg/model-aliases` |
//! | `RS-5-054` | `POST  ` | `/api/cfg/model-aliases` | `/api/cfg/model-aliases` |
//! | `RS-5-055` | `DELETE` | `/api/cfg/model-aliases` | `/api/cfg/model-aliases` |
//!
//! **Status: not ported.** This file is the slot; the endpoint batch that owns
//! `routes/cfg.py` fills in the handlers and the [`register`] body, and
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
