//! The React build: the `/static` mount and the four SPA page routes.
//!
//! These live in `server.py` itself rather than in a `routes/*.py` module
//! (DRIFT-5), and they are the reason wave 5's oracle works at all: the
//! dashboard is the *unmodified* checked-in bundle under
//! `stackunderflow/static/react/`, so if it renders against this server the
//! endpoints below it are right in the only way that matters
//! (`docs/specs/rust-port.md` §2.3).
//!
//! ```python
//! app.mount("/static", StaticFiles(directory=os.path.join(BASE_DIR, "static")), name="static")
//! @app.get("/")                        -> FileResponse(static/react/index.html)
//! @app.get("/project/{full_path:path}")-> FileResponse(static/react/index.html)
//! @app.get("/settings")                -> FileResponse(static/react/index.html)
//! @app.get("/live")                    -> FileResponse(static/react/index.html)
//! ```
//!
//! All four serve the same bytes; the SPA router does the rest client-side.
//!
//! # Recorded header gap (DIV-051)
//!
//! starlette's `FileResponse.set_stat_headers` also emits `last-modified`
//! (`email.utils.formatdate(st_mtime, usegmt=True)`) and an `etag` of
//! `md5(f"{st_mtime}-{st_size}")` in quotes. Both are reproducible — the inputs
//! are just `stat()` — but neither is ported here: it would cost an md5
//! implementation and an RFC-1123 date formatter for headers the parity differ
//! does not compare and the bundle does not need (their only effect is `304`
//! revalidation). `content-type`, `content-length` and `accept-ranges` *are*
//! ported, because the first is the contract and the other two are free.

use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use tower::util::MapResponseLayer;
use tower_http::services::ServeDir;

use crate::state::AppState;

/// What `mimetypes.guess_type("index.html")` returns, plus the `charset` that
/// `Response.init_headers` appends to every `text/*` media type.
const HTML_CONTENT_TYPE: &str = "text/html; charset=utf-8";

/// Mount `/static` over `BASE_DIR/static` — `app.mount(...)`, the first thing
/// `server.py` does after the middleware.
///
/// `ServeDir` is the `StaticFiles` equivalent: same directory, same
/// path-traversal refusal, same `mime_guess` content types. Two things it does
/// *not* match out of the box, both found by the differ rather than by reading:
///
/// * **`charset=utf-8` on `text/*`.** `StaticFiles` guesses the media type and
///   then hands the response to starlette's `Response.init_headers`, which
///   appends `; charset=utf-8` to anything starting `text/`. `ServeDir` stops at
///   the guess, so `index.html` went out as `text/html` against Python's
///   `text/html; charset=utf-8`. [`add_text_charset`] restores it.
/// * **The 404 body.** A miss under the mount is `{"detail":"Not Found"}` in
///   FastAPI, not an empty 404.
pub fn register_static(router: Router<AppState>, state: &AppState) -> Router<AppState> {
    let missing = axum::routing::any(|| async { crate::json::not_found() });
    let files = ServeDir::new(state.static_dir()).not_found_service(missing);
    // Wrapped in a `Router` before the layer so the request body type is pinned
    // to `axum::body::Body`; `ServeDir::map_response` alone leaves it generic
    // and `nest_service` cannot then infer it.
    let mount: Router<()> = Router::new()
        .fallback_service(files)
        .layer(MapResponseLayer::new(add_text_charset));
    router.nest_service("/static", mount)
}

/// starlette's `Response.init_headers`: `text/*` without a charset gains
/// `; charset=utf-8`. Nothing else is touched.
fn add_text_charset(mut response: http::Response<Body>) -> http::Response<Body> {
    let current = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    if let Some(current) = current
        && current.starts_with("text/")
        && !current.contains("charset=")
        && let Ok(patched) = HeaderValue::from_str(&format!("{current}; charset=utf-8"))
    {
        response.headers_mut().insert(header::CONTENT_TYPE, patched);
    }
    response
}

/// Mount the four SPA page routes, in `server.py`'s declaration order.
///
/// `{full_path:path}` becomes axum's `{*full_path}`: both mean "the rest of the
/// path, slashes included". The capture is unused — Python's handler ignores it
/// too — but the route must still exist, because `/project/foo/bar` is a real
/// URL the dashboard puts in the address bar.
pub fn register_pages(router: Router<AppState>) -> Router<AppState> {
    router
        .route("/", get(spa_index))
        .route("/project/{*full_path}", get(spa_index))
        .route("/settings", get(spa_index))
        .route("/live", get(spa_index))
}

/// `FileResponse(os.path.join(BASE_DIR, "static", "react", "index.html"))`.
async fn spa_index(State(state): State<AppState>) -> Response {
    let path = state.spa_index();
    match tokio::fs::read(&path).await {
        Ok(bytes) => {
            let len = bytes.len();
            (
                StatusCode::OK,
                [
                    (
                        header::CONTENT_TYPE,
                        HeaderValue::from_static(HTML_CONTENT_TYPE),
                    ),
                    (header::ACCEPT_RANGES, HeaderValue::from_static("bytes")),
                ],
                [(
                    header::CONTENT_LENGTH,
                    HeaderValue::from_str(&len.to_string())
                        .unwrap_or_else(|_| HeaderValue::from_static("0")),
                )],
                Body::from(bytes),
            )
                .into_response()
        }
        // starlette raises `RuntimeError: File at path … does not exist.` and
        // the ASGI server turns that into a 500. A missing bundle is a broken
        // install, not a route that should 404 into the SPA.
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            [(
                header::CONTENT_TYPE,
                HeaderValue::from_static(crate::json::JSON_CONTENT_TYPE),
            )],
            Body::from(stax_memory::pyjson::dumps_http(&serde_json::json!({
                "detail": format!("File at path {} does not exist. ({err})", path.display()),
            }))),
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::Config;
    use std::path::PathBuf;

    fn repo_package() -> PathBuf {
        // …/StackUnderflow-rust/rust/crates/stax-server → …/stackunderflow
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../../stackunderflow")
            .canonicalize()
            .expect("the checked-in package tree is the wave-5 oracle")
    }

    #[test]
    fn the_react_oracle_is_present_and_tracked() {
        // If this fails the whole wave has no oracle. It must never be
        // "fixed" by running a frontend build — the bundle is checked in and
        // stays unmodified (§2.3).
        let index = repo_package()
            .join("static")
            .join("react")
            .join("index.html");
        assert!(index.is_file(), "missing {}", index.display());
        let html = std::fs::read_to_string(&index).expect("readable");
        assert!(html.contains("<div id=\"root\">"), "not the SPA entry");
    }

    #[tokio::test]
    async fn every_page_route_serves_the_same_index_bytes() {
        use axum::body::to_bytes;
        use axum::http::Request;
        use tower::ServiceExt as _;

        let package = repo_package();
        let expected =
            std::fs::read(package.join("static").join("react").join("index.html")).expect("read");
        let state = AppState::new(
            package.join("does-not-exist.db"),
            package,
            Config::default(),
        );
        let app = crate::app(state);

        for path in [
            "/",
            "/settings",
            "/live",
            "/project/-home-u-repo",
            "/project/a/b",
        ] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(path)
                        .body(Body::empty())
                        .expect("request"),
                )
                .await
                .expect("response");
            assert_eq!(response.status(), StatusCode::OK, "{path}");
            assert_eq!(
                response
                    .headers()
                    .get(header::CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok()),
                Some(HTML_CONTENT_TYPE),
                "{path}"
            );
            let body = to_bytes(response.into_body(), usize::MAX)
                .await
                .expect("body");
            assert_eq!(body.as_ref(), expected.as_slice(), "{path}");
        }
    }

    #[tokio::test]
    async fn the_static_mount_serves_the_bundles_assets() {
        use axum::http::Request;
        use tower::ServiceExt as _;

        let package = repo_package();
        let state = AppState::new(
            package.join("does-not-exist.db"),
            package,
            Config::default(),
        );
        let app = crate::app(state);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/static/react/index.html")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::OK);
    }
}
