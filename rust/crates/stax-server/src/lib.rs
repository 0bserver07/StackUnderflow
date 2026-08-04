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

pub mod assets;
pub mod cors;
pub mod currency;
pub mod json;
pub mod method_semantics;
pub mod path_semantics;
pub mod qs;
pub mod routes;
pub mod services;
pub mod spa;
pub mod state;

// WAVE 8 TRANCHE 3 — `pricing` and `pyops` moved to `stax-reports`, which owns
// the report layer both this crate and `stax-cli` consume (see that crate's
// `lib.rs` for why). Re-exported rather than re-spelled so every `crate::pricing`
// / `crate::pyops` path already written in `routes/` and `services/` still names
// the same items: the split cost the route modules zero edits, which is the
// point of doing it as a move.
pub use stax_reports::{pricing, pyops};

use axum::Router;
// `Layer::layer` is what puts `method_semantics` OUTSIDE the router rather than
// inside it; `Router::layer` would run after matching. See `app()`.
use tower::Layer as _;

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
/// # The three layers, and why they are in this order
///
/// starlette wraps its router in `ServerErrorMiddleware → user middleware →
/// ExceptionMiddleware → Router`, so the stack below is that stack:
///
/// 1. [`cors`] — `CORSMiddleware`, the one `add_middleware` call in
///    `server.py`. Outermost, which is not cosmetic: a preflight short-circuits
///    *before* routing, so `OPTIONS /api/health/` answers `200 OK` and never
///    redirects, and the simple-request headers land on the `307` when it does.
///    DIV-050, ruled and ported.
/// 2. [`path_semantics`] — uvicorn's percent-decode (DIV-168) and starlette's
///    `redirect_slashes` `307` (DIV-133, and DIV-361 with it). Both have to
///    happen around the matcher: the decode before it, the redirect after it
///    has failed.
/// 3. [`method_semantics`] — axum aliases `HEAD` onto `GET` and reports every
///    registered method in `Allow`; FastAPI does neither. DIV-323.
///
/// Each is `Layer::layer`ed onto the finished router rather than
/// `Router::layer`ed into it, because `Router::layer` runs *after* routing and
/// two of the three rules have to be earlier than that.
///
/// The `fallback` stamps [`path_semantics::Unmatched`] onto its response. That
/// marker is the whole of how layer 2 tells "no route claimed this path" (which
/// is what starlette redirects on) apart from "a handler answered `404`" (which
/// it does not) — without either duplicating the route table or guessing from a
/// status code.
pub fn app(state: AppState) -> Router {
    let router = Router::new();
    let router = spa::register_static(router, &state);
    let router = routes::register_all(router);
    let router = spa::register_pages(router);
    let cors = cors::CorsPolicy::from_config(state.config());
    let routed = router
        // FastAPI replaces starlette's plain-text 404/405 with JSON ones. axum's
        // defaults are an empty body and no `content-type` at all, which is a
        // divergence on every unknown path — the differ caught it on the first
        // run, on a case that was only in the file to prove the harness worked.
        .fallback(unmatched_path)
        .method_not_allowed_fallback(|| async { json::method_not_allowed() })
        .with_state(state);

    // The probe handle layer 2 asks "does this path exist?" — the very same
    // router, so it cannot drift from the one that answers the request.
    let probe = routed.clone();

    // Everything — routes, both fallbacks, the `/static` mount — behind the
    // layers, reached through a stateless outer router so `app()` still returns
    // a `Router` and no caller changes.
    let methods =
        axum::middleware::from_fn(method_semantics::python_method_semantics).layer(routed);
    let paths = axum::middleware::from_fn(move |req, next| {
        path_semantics::python_path_semantics(
            probe.clone(),
            Some(path_semantics::STATIC_MOUNT),
            req,
            next,
        )
    })
    .layer(methods);
    let cors =
        axum::middleware::from_fn(move |req, next| cors::python_cors(cors.clone(), req, next))
            .layer(paths);

    Router::new().fallback_service(cors)
}

/// The app fallback: FastAPI's `404` body, plus the marker layer 2 reads.
///
/// Split out of the closure it used to be so the marker has somewhere to live.
/// It is a response *extension*, which never reaches the wire — the bytes are
/// byte-for-byte what they were.
async fn unmatched_path() -> axum::response::Response {
    use axum::response::IntoResponse as _;
    let mut res = json::not_found().into_response();
    res.extensions_mut().insert(path_semantics::Unmatched);
    res
}

/// `cli.py::ingest_webhook_serve_cmd`'s app — the PR/CI receiver, alone.
///
/// ```python
/// app = FastAPI(title="StackUnderflow webhook receiver")
/// app.include_router(webhook_router)
/// ```
///
/// Three routes and nothing else: no SPA, no `/static`, no dashboard API. That
/// is the point of the verb — a receiver you can expose to a tunnel without
/// exposing the dashboard with it, on its own port.
///
/// The two fallbacks and the method layer stay, because they are not the
/// dashboard's: they are what FastAPI does on *any* app. An unknown path here
/// is `{"detail":"Not Found"}` on both implementations, and a `GET` on
/// `/api/webhooks/github` is a 405 with FastAPI's `Allow` semantics, not axum's
/// — the same DIV-323 rule, and the receiver differ's rows cross it.
///
/// [`path_semantics`] joins them for the same reason: `redirect_slashes` is a
/// property of every starlette `Router`, so `/api/webhooks/github/` is a `307`
/// here too. What it is **not** given is a mount root — this app has no
/// `/static`, and a receiver that answered `307` to `/static` would be
/// inventing one. Nor does it get [`cors`]: `cli.py::ingest_webhook_serve_cmd`
/// builds a bare `FastAPI()` with no `add_middleware` call at all.
///
/// Appended below [`app`] rather than folded into it: `app` is shared ground and
/// this leg may not re-shape it.
pub fn webhook_receiver_app(state: AppState) -> Router {
    let routed = routes::webhooks::register(Router::new())
        .fallback(unmatched_path)
        .method_not_allowed_fallback(|| async { json::method_not_allowed() })
        .with_state(state);
    let probe = routed.clone();
    let methods =
        axum::middleware::from_fn(method_semantics::python_method_semantics).layer(routed);
    Router::new().fallback_service(
        axum::middleware::from_fn(move |req, next| {
            path_semantics::python_path_semantics(probe.clone(), None, req, next)
        })
        .layer(methods),
    )
}
