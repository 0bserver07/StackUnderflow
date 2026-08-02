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
use axum::body::{Body, Bytes};
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
///
/// Since wave-10 item 2c the *default* source is the binary — see
/// [`serve_embedded`], which reproduces the four `ServeDir` behaviours the
/// mount actually exhibits. `STAX_STATIC_DIR` puts `ServeDir` back, unchanged,
/// over the directory it names.
pub fn register_static(router: Router<AppState>, state: &AppState) -> Router<AppState> {
    let missing = axum::routing::any(|| async { crate::json::not_found() });
    // Wrapped in a `Router` before the layer so the request body type is pinned
    // to `axum::body::Body`; `ServeDir::map_response` alone leaves it generic
    // and `nest_service` cannot then infer it.
    let mount: Router<()> = match state.static_dir_override() {
        Some(dir) => Router::new().fallback_service(ServeDir::new(dir).not_found_service(missing)),
        None => Router::new().fallback(serve_embedded),
    }
    .layer(MapResponseLayer::new(add_text_charset));
    router.nest_service("/static", mount)
}

/// The compiled-in mount — `ServeDir` over [`crate::assets`] instead of a
/// directory.
///
/// Every branch below was **measured against the running `ServeDir`** before it
/// was written (the probe transcript is in the wave-10 packaging item), and
/// then checked against `tower-http-0.6.11`'s source, because a mount that
/// merely "looks right" would move seven case rows:
///
/// | request | answer |
/// |---|---|
/// | a file | `200`, `mime_guess` type + `accept-ranges: bytes` + `content-length` |
/// | `HEAD` of a file | the same headers, no body |
/// | a directory without a trailing slash | `307` to the same URI + `/`, empty |
/// | a directory with one | its `index.html`, or the miss below |
/// | a miss, or a `..`/absolute component | the mount's `not_found_service` — `404 {"detail":"Not Found"}` |
/// | anything not `GET`/`HEAD` | `405` + `allow: GET,HEAD`, empty, no `content-type` |
///
/// The last row is reachable **only at the bare mount root**: the app-wide
/// `method_semantics` layer answers `/static/…` first (DIV-360). It is
/// reproduced anyway because `!AL-static-root-put` measures exactly those bytes
/// every run.
///
/// # The one header that could not survive (DIV-402)
///
/// `last-modified`. `ServeDir` reads it from `stat()`; an embedded slice has no
/// mtime. The differ has never compared it (status, `content-type`, `allow` and
/// the body are the contract — `parity/src/endpoints.rs`), and DIV-051 already
/// records the SPA routes dropping the same header for the same reason.
/// `accept-ranges: bytes` is kept because it is what the mount has always
/// advertised, but `Range` is not honoured here — a ranged request gets the
/// whole file, which is a legal answer and an unmeasured one.
async fn serve_embedded(request: axum::extract::Request) -> Response {
    if request.method() != http::Method::GET && request.method() != http::Method::HEAD {
        return (
            StatusCode::METHOD_NOT_ALLOWED,
            [(header::ALLOW, HeaderValue::from_static("GET,HEAD"))],
            Body::empty(),
        )
            .into_response();
    }

    let uri = request.uri();
    let Some(mut key) = validate_key(uri.path()) else {
        return crate::json::not_found().into_response();
    };

    if crate::assets::is_dir(&key) {
        // `maybe_redirect_or_append_path`: a directory URI that does not end in
        // `/` redirects rather than serving, and the redirect is built from the
        // URI this service sees — which `nest_service` has already stripped of
        // `/static`, so the `location` comes out prefix-less. That is what the
        // mount has always done; reproduced, not corrected.
        if !uri.path().is_empty() && !uri.path().ends_with('/') {
            let location = match uri.query() {
                Some(query) => format!("{}/?{query}", uri.path()),
                None => format!("{}/", uri.path()),
            };
            let Ok(location) = HeaderValue::from_str(&location) else {
                return crate::json::not_found().into_response();
            };
            return (
                StatusCode::TEMPORARY_REDIRECT,
                [(header::LOCATION, location)],
                Body::empty(),
            )
                .into_response();
        }
        if key.is_empty() {
            key = "index.html".to_owned();
        } else {
            key.push_str("/index.html");
        }
    }

    let Some(bytes) = crate::assets::get(&key) else {
        return crate::json::not_found().into_response();
    };
    // `mime_guess::from_path(...).first_raw()`, defaulting to
    // `application/octet-stream` — `ServeDir`'s exact resolution. The
    // `add_text_charset` layer above then restores starlette's charset, as it
    // always has.
    let media = mime_guess::from_path(&key)
        .first_raw()
        .unwrap_or("application/octet-stream");
    let body = if request.method() == http::Method::HEAD {
        Body::empty()
    } else {
        Body::from(Bytes::from_static(bytes))
    };
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, HeaderValue::from_static(media)),
            (header::ACCEPT_RANGES, HeaderValue::from_static("bytes")),
        ],
        // Set explicitly rather than left to the body: a `HEAD` answers with the
        // full file's length over an empty body, which is what `ServeDir` does
        // (`FileRequestExtent::Head(meta)` keeps `meta.len()`).
        [(
            header::CONTENT_LENGTH,
            HeaderValue::from_str(&bytes.len().to_string())
                .unwrap_or_else(|_| HeaderValue::from_static("0")),
        )],
        body,
    )
        .into_response()
}

/// `ServeVariant::Directory::build_and_validate_path`, in the key space.
///
/// Percent-decode, then walk components: `.` is dropped, a normal component is
/// kept, and **anything else — `..`, a root, a Windows prefix — fails the whole
/// path**. Failing is `InvalidFilename`, which `ServeDir` routes to the same
/// fallback a miss uses, so the caller answers both with `404`.
fn validate_key(path: &str) -> Option<String> {
    let decoded = percent_decode(path.trim_start_matches('/'))?;
    let mut key = String::with_capacity(decoded.len());
    for component in std::path::Path::new(&decoded).components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::Normal(part) => {
                if !key.is_empty() {
                    key.push('/');
                }
                key.push_str(&part.to_string_lossy());
            }
            _ => return None,
        }
    }
    Some(key)
}

/// `percent_encoding::percent_decode(...).decode_utf8()`, in fifteen lines.
///
/// The crate is in the lock (`ServeDir` uses it) but is not a dependency of
/// this one, and the manifest law says measure before taking: this is the whole
/// of what is needed. A stray `%` or a bad hex pair is passed through
/// literally, which is what `percent-encoding` does; invalid UTF-8 fails the
/// path, which is what `decode_utf8()` does.
fn percent_decode(raw: &str) -> Option<String> {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(hi), Some(lo)) = (
                (bytes[i + 1] as char).to_digit(16),
                (bytes[i + 2] as char).to_digit(16),
            )
        {
            #[allow(
                clippy::cast_possible_truncation,
                reason = "a hex digit pair is one byte by construction"
            )]
            out.push((hi * 16 + lo) as u8);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
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
    match state.read_static(&path).await {
        Some(bytes) => {
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
        None => (
            StatusCode::INTERNAL_SERVER_ERROR,
            [(
                header::CONTENT_TYPE,
                HeaderValue::from_static(crate::json::JSON_CONTENT_TYPE),
            )],
            Body::from(stax_memory::pyjson::dumps_http(&serde_json::json!({
                // starlette's own text, verbatim. The port used to append the
                // `io::Error` in parentheses; `read_static` answers `Option`
                // (an embedded miss has no errno to append), so the addendum is
                // gone and the message is now exactly the reference's —
                // DIV-403, a strictly closer body on a leg no case row reaches,
                // because the bundle is compiled in and cannot be missing.
                "detail": format!("File at path {} does not exist.", path.display()),
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
