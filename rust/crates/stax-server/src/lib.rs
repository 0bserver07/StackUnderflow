//! The HTTP surface: axum, 93-endpoint parity, and the existing React build.
//!
//! Charter (`docs/specs/rust-port.md` §3): port the Python route modules — all
//! 93 endpoints, same paths, same query parameters, same response shapes — on
//! axum, and serve the unmodified React bundle from `stackunderflow/static/react/`.
//! That untouched frontend is the parity oracle (§2.3): the dashboard must work
//! against this server with no client change, which per §6b.5 means inheriting
//! the sign-inverted timezone offsets the current React callers send until the
//! frontend fix lands and both flip together.
//!
//! # What wave 5's foundation fixes in place
//!
//! * **Composition mirrors `server.py`.** [`routes`] holds one module per
//!   `routes/*.py`, listed in the exact `include_router` order (34 of them —
//!   DRIFT-1 measured that; the spec said 12). Each exposes
//!   `register(Router<AppState>) -> Router<AppState>`, so an endpoint batch
//!   edits one file and no other.
//! * **State is injected, never global.** [`state::AppState`] carries the store
//!   path, the static root, the resolved settings and the mutable "current
//!   project" that `deps.py` keeps in module scope. Finding 5 of
//!   `rust/ARCHITECT-STATE.md` makes this law: `std::env::set_var` is `unsafe`
//!   in Rust 2024 and the workspace forbids `unsafe`, so configuration is a
//!   pure function of injected inputs.
//! * **Bodies go out through CPython's writer, not `serde_json`'s.**
//!   [`json::JsonBody`] renders with [`stax_memory::pyjson::dumps_http`] —
//!   starlette's `JSONResponse.render` flags exactly (`ensure_ascii=False`,
//!   `separators=(",", ":")`) and CPython's `repr(float)`. ryu, which
//!   `serde_json` uses by default, is a *third* renderer: it writes `1e16`
//!   where Python writes `1e+16` and `1e-5` where Python writes `1e-05`. The
//!   digits agree; the bytes do not, and bytes are the contract.
//!
//! Everything blocking (SQLite, directory globs) runs on
//! `tokio::task::spawn_blocking`, which is the same bargain FastAPI strikes
//! with `run_in_threadpool` and plain `def` handlers.

#![forbid(unsafe_code)]

pub mod currency;
pub mod json;
pub mod pricing;
pub mod pyops;
pub mod qs;
pub mod routes;
pub mod services;
pub mod spa;
pub mod state;

use axum::Router;

pub use state::{AppState, Config};

/// Build the whole application — the Rust half of `server.py`'s module body.
///
/// Order is `server.py`'s: the `/static` mount, then the 34 routers in
/// `include_router` order, then the SPA page routes declared after them. axum
/// does not care (its matcher is order-independent and panics on a genuine
/// duplicate rather than silently shadowing it), but the order is the record of
/// which handler Starlette would have picked, so it is reproduced rather than
/// sorted.
///
/// What is deliberately *not* here: `CORSMiddleware`. It only ever adds headers
/// to a cross-origin request, the parity differ is same-origin, and porting it
/// blind would mean inventing a header order nothing has measured. Recorded as
/// DIV-050, not skipped quietly.
pub fn app(state: AppState) -> Router {
    let router = Router::new();
    let router = spa::register_static(router, &state);
    let router = routes::register_all(router);
    let router = spa::register_pages(router);
    router
        // FastAPI replaces starlette's plain-text 404/405 with JSON ones. axum's
        // defaults are an empty body and no `content-type` at all, which is a
        // divergence on every unknown path — the differ caught it on the first
        // run, on a case that was only in the file to prove the harness worked.
        .fallback(|| async { json::not_found() })
        .method_not_allowed_fallback(|| async { json::method_not_allowed() })
        .with_state(state)
}
