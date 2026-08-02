//! Python's method semantics — DIV-323: `HEAD` is not `GET`, and `Allow` names
//! exactly one method.
//!
//! # The two divergences, as measured
//!
//! Both were measured against the reference on `.parity-state/fresh`
//! (raw sockets, no HTTP client library — same reason `parity/src/http.rs` is
//! hand-rolled) before a line of this file was written. They are *not* what a
//! reading of the two frameworks' docs would have predicted, which is why the
//! numbers are transcribed here rather than summarised:
//!
//! | request | reference | port, before |
//! |---|---|---|
//! | `HEAD /api/health` | `405`, `allow: GET` | `200`, `content-length: 96` |
//! | `HEAD /api/budgets` | `405`, `allow: GET` | `200`, `content-length: 136` |
//! | `HEAD /` (SPA) | `405`, `allow: GET` | `200`, `text/html` |
//! | `HEAD /static/react/index.html` | `200` | `200` — **already identical** |
//! | `HEAD /api/nope` | `404` | `404` — already identical |
//! | `POST /api/health` | `405`, `allow: GET` | `405`, `allow: GET,HEAD` |
//! | `PATCH /api/budgets` | `405`, `allow: GET` | `405`, `allow: GET,HEAD,PUT,DELETE` |
//!
//! ## Why the reference answers that way
//!
//! Starlette's `Route` adds `HEAD` whenever `GET` is present — but FastAPI's
//! `APIRoute` does not, and every route in `stackunderflow/routes/` is a
//! FastAPI one. So on the reference `HEAD` is an unclaimed method on a claimed
//! path, which is a `405`. The `/static` mount is the exception: it is a plain
//! starlette `StaticFiles`, which serves `HEAD` itself.
//!
//! The `Allow` value is the surprise. Starlette's router walks its routes in
//! declaration order and keeps the **first** `Match.PARTIAL` — the first route
//! whose *path* matches but whose method does not — then answers with
//! `", ".join(that_route.methods)`. Every FastAPI route declares exactly one
//! method, so the reference's `Allow` is always a **single token**: the method
//! of the first-declared route on that path. `/api/budgets` carries `GET`,
//! `PUT` and `DELETE` in `budgets.py` in that order, and the reference answers
//! `allow: GET` to a `PATCH` — not `GET, PUT, DELETE`.
//!
//! axum instead reports every method registered on the path, and inserts the
//! synthetic `HEAD` it aliases onto `GET`.
//!
//! ## The port
//!
//! Two rules, applied by one layer that sits **outside** the router (so the
//! method rewrite happens before matching — `Router::layer` runs *after*
//! routing and would have been too late):
//!
//! 1. A `HEAD` outside the `/static` mount is re-dispatched under a method no
//!    route registers, so axum's own matcher produces either its `404` (path
//!    unknown — which the reference also answers) or its `405` plus an `Allow`
//!    naming the path's registered set. Nothing else can tell us "does this
//!    path exist, and with which methods" without duplicating the route table.
//! 2. Any `405` leaving the app has its `Allow` rewritten to the reference's
//!    single token: drop the synthetic `HEAD`, keep the **first** remaining
//!    method. axum lists methods in registration order and the route modules
//!    register in the Python module's declaration order (the wave-5 law), so
//!    "first registered" *is* "first declared".
//!
//! The body is already right: `method_not_allowed_fallback` renders FastAPI's
//! `{"detail":"Method Not Allowed"}`, and hyper strips the body of a `HEAD`
//! response while keeping the `content-length` — exactly what uvicorn does.
//!
//! ## The one case this could not reach — CLOSED, and not by this file
//!
//! `/api/project` declares `POST` first and `GET` second in `projects.py`, so
//! the reference answers `allow: POST`. The port registered only the `GET` (the
//! `POST` was unported — **DIV-341**) and therefore answered `allow: GET`. That
//! was never a defect of this layer: a router cannot name a method it does not
//! have.
//!
//! `RS-5-095` landed the `POST` on 2026-08-01 and the row (`H-head-project`)
//! went green with **no edit here**. Rule 2 keeps the first non-`HEAD` token of
//! axum's `Allow`, axum lists methods in registration order, and
//! `routes::projects::register` now spells
//! `post(set_project).get(get_current_project)` in one `MethodRouter`. So the
//! wave-5 law — *register in the Python module's declaration order* — is not a
//! tidiness convention: it is the input this rule computes from, and
//! `H-head-project` is the one row in 735 that would notice it being broken.

use axum::extract::Request;
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse as _, Response};

/// The prefix `app.mount("/static", StaticFiles(...), name="static")` claims.
///
/// The reference serves `GET` and `HEAD` here — `StaticFiles` is starlette's,
/// not FastAPI's — so those two pass through untouched. Measured: both
/// implementations already answer `HEAD /static/react/index.html` with `200`
/// and `HEAD /static/react/nope.js` with `404`.
const STATIC_MOUNT: &str = "/static";

/// A method no route in the app registers, used to ask axum's matcher "does
/// this path exist, and with which methods?".
///
/// `TRACE` and nothing else: the app registers `GET`, `POST`, `PUT` and
/// `DELETE` only, so `TRACE` can never reach a handler — it lands on
/// `method_not_allowed_fallback` (path known) or the `404` fallback (path
/// unknown), which is precisely the discrimination a `HEAD` needs.
const PROBE_METHOD: Method = Method::TRACE;

/// Is this path the mount root or inside it?
#[must_use]
fn under_static_mount(path: &str) -> bool {
    path == STATIC_MOUNT || in_static_subtree(path)
}

/// Is this path *inside* the mount — a file, not the mount root itself?
///
/// The distinction is load-bearing and measured. Inside the subtree the
/// reference runs `StaticFiles.get_response`, whose first act is
/// `if scope["method"] not in ("GET", "HEAD"): raise HTTPException(405)`.
/// The bare mount root never reaches it: starlette's `Mount` answers a **307**
/// to `/static/` on *every* method, which this port does not do — **DIV-361**,
/// the `/static` face of DIV-133's redirect family, and the architect's the
/// same way DIV-133 is. This layer deliberately leaves the mount root alone so
/// that finding stays exactly as measured.
#[must_use]
fn in_static_subtree(path: &str) -> bool {
    path.starts_with("/static/")
}

/// Starlette's `Allow` from axum's: drop the synthetic `HEAD`, keep the first.
///
/// Returns `None` when there is nothing to say — an absent header, or a set
/// that is `HEAD` and nothing else (which this app never produces, but a future
/// `head()` registration would).
#[must_use]
fn starlette_allow(axum_allow: Option<&HeaderValue>) -> Option<HeaderValue> {
    let raw = axum_allow?.to_str().ok()?;
    let first = raw
        .split(',')
        .map(str::trim)
        .find(|m| !m.is_empty() && !m.eq_ignore_ascii_case("HEAD"))?;
    HeaderValue::from_str(first).ok()
}

/// The layer. Wraps the whole app, outside routing.
pub async fn python_method_semantics(mut req: Request, next: Next) -> Response {
    let path = req.uri().path();
    let under_static = under_static_mount(path);

    // Rule 3 — DIV-360, the `/static` subtree's 405 shape. `StaticFiles` raises
    // `HTTPException(405)` for anything that is not `GET` or `HEAD`, so FastAPI's
    // handler renders it: JSON body, and — because the exception carries no
    // headers — **no `Allow` header at all**. tower-http's `ServeDir` instead
    // answers a bare `405` with `allow: GET,HEAD`, no `content-type` and an empty
    // body. Measured on three paths, a missing file among them; it predates this
    // layer and no case row had ever sent a non-`GET` here.
    if in_static_subtree(path) && req.method() != Method::GET && req.method() != Method::HEAD {
        return crate::json::method_not_allowed().into_response();
    }

    // Rule 1 — a HEAD outside the mount is an unclaimed method on the
    // reference, so it must be an unclaimed method here.
    if !under_static && req.method() == Method::HEAD {
        *req.method_mut() = PROBE_METHOD;
    }

    let mut res = next.run(req).await;

    // Rule 2 — the `Allow` value. Not applied inside the mount: rule 3 already
    // owns every method that could produce a 405 there.
    if !under_static
        && res.status() == StatusCode::METHOD_NOT_ALLOWED
        && let Some(allow) = starlette_allow(res.headers().get(header::ALLOW))
    {
        res.headers_mut().insert(header::ALLOW, allow);
    }

    res
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_mount_is_matched_on_a_segment_boundary() {
        assert!(under_static_mount("/static"));
        assert!(under_static_mount("/static/react/index.html"));
        // Not the mount: a sibling path that merely shares a prefix. Getting
        // this wrong would hand `HEAD /statics` starlette's file semantics.
        assert!(!under_static_mount("/statics"));
        assert!(!under_static_mount("/api/static"));
        assert!(!under_static_mount("/"));
    }

    #[test]
    fn the_mount_root_is_not_the_subtree() {
        // DIV-361's boundary. The root is a `Mount` 307 on the reference and is
        // left alone here; the subtree is `StaticFiles` and rule 3 owns it.
        assert!(!in_static_subtree("/static"));
        assert!(in_static_subtree("/static/"));
        assert!(in_static_subtree("/static/react/nope.js"));
        assert!(!in_static_subtree("/statics/x"));
    }

    #[test]
    fn the_synthetic_head_is_dropped_and_only_the_first_method_survives() {
        // `/api/budgets`, measured: axum says this, the reference says `GET`.
        let axum = HeaderValue::from_static("GET,HEAD,PUT,DELETE");
        assert_eq!(
            starlette_allow(Some(&axum)).expect("a method survives"),
            "GET"
        );
    }

    #[test]
    fn a_post_only_path_is_unchanged() {
        // `/api/project-by-dir`, measured identical on both sides already.
        let axum = HeaderValue::from_static("POST");
        assert_eq!(starlette_allow(Some(&axum)).expect("unchanged"), "POST");
    }

    #[test]
    fn a_get_only_path_loses_its_head() {
        let axum = HeaderValue::from_static("GET,HEAD");
        assert_eq!(starlette_allow(Some(&axum)).expect("GET"), "GET");
    }

    #[test]
    fn spacing_and_case_are_tolerated() {
        let axum = HeaderValue::from_static("head, get, put");
        assert_eq!(starlette_allow(Some(&axum)).expect("get"), "get");
    }

    #[test]
    fn nothing_to_say_stays_nothing() {
        assert!(starlette_allow(None).is_none());
        // A hypothetical HEAD-only route: there is no first non-HEAD method, so
        // the layer leaves axum's own header rather than inventing one.
        let head_only = HeaderValue::from_static("HEAD");
        assert!(starlette_allow(Some(&head_only)).is_none());
    }
}
