//! `routes/misc.py` — 6 endpoints, wave 5 (batch A, completed in batch E).
//!
//! | Item | Method | FastAPI path | axum path | State |
//! |---|---|---|---|---|
//! | `RS-5-079` | `GET`  | `/api/pricing`             | `/api/pricing`          | ported — no case row, ever |
//! | `RS-5-080` | `POST` | `/api/pricing/refresh`     | `/api/pricing/refresh`  | ported — no case row, ever |
//! | `RS-5-081` | `GET`  | `/api/health`              | `/api/health`           | ported |
//! | `RS-5-082` | `GET`  | `/favicon.ico`             | `/favicon.ico`          | ported |
//! | `RS-5-083` | `GET`  | `/assets/{full_path:path}` | `/assets/{*full_path}`  | ported |
//! | `RS-5-084` | `GET`… | `/ollama-api/{path:path}`  | `/ollama-api/{*path}`   | ported — `M-ollama` flipped |
//!
//! # What is ported, and what is a network call wearing a route's clothes
//!
//! Three of these six read the filesystem and answer. The other three do not
//! answer from local state at all, and each one needed a *measurement* of the
//! machine before a line of it could be written honestly.
//!
//! * `/ollama-api/{path}` proxies to `http://localhost:11434` (DIV-066). Port
//!   11434 is **closed on this host** — `ss -lnt` lists no listener, a TCP
//!   connect to `127.0.0.1:11434` and to `[::1]:11434` is refused, and `getent
//!   ahosts localhost` resolves to `127.0.0.1` only. The only branch either side
//!   can reach is the bare `except Exception`, whose body is a fixed
//!   `502 {"error":"Ollama not available"}`. That is deterministic, so
//!   `M-ollama` stops being a `!` row and becomes a real one — see
//!   [`crate::services::ollama_proxy`] for what changes the day Ollama is up.
//! * `/api/pricing` calls `PricingService.get_pricing()`, which on a cache older
//!   than 24 h issues a blocking `urlopen` to LiteLLM on GitHub and **writes the
//!   fetched payload back to `$STACKUNDERFLOW_HOME/cache/pricing.json`**.
//! * `/api/pricing/refresh` is the same fetch, unconditionally, plus the write.
//!
//! Both pricing routes are now ported — see [`crate::services::pricing_refresh`],
//! which also records the one thing the port cannot do (the LiteLLM URL is
//! HTTPS and this workspace has no TLS crate, so the Rust half is pinned to the
//! reference's fetch-failure leg). Neither gets a row in
//! `parity/endpoint-cases.txt`, and that exception was earned rather than
//! assumed. They went in as `!` rows first, and the run proved the problem: a
//! `!` row still ISSUES the request — the marker only softens the verdict — so
//! Python fetched LiteLLM and wrote `cache/pricing.json` into the shared home,
//! which turned all five `GET /api/pricing/doctor` rows from a deterministic "no
//! overlay on disk" payload into a live elapsed-time `age_days` float. Five clean
//! cases became five divergences on one field, from a case file edit, with no
//! code change. An endpoint whose side effect is another endpoint's input cannot
//! share a home with it, so the two of them are verified by the isolated
//! procedure in `rust/PRICING-REFRESH-DIFFER.md` instead.

use std::path::{Path, PathBuf};

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{Path as PathParam, Request, State};
use axum::http::{HeaderName, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde_json::{Map, Value};

use crate::json::{JSON_CONTENT_TYPE, JsonBody};
use crate::services::ollama_proxy::{self, ProxyOutcome};
use crate::services::pricing_refresh::{PricingService, rate_card_payload};
use crate::state::AppState;

/// Mount this module's endpoints onto `router`.
pub fn register(router: Router<AppState>) -> Router<AppState> {
    router
        .route("/api/pricing", get(get_pricing))
        .route("/api/pricing/refresh", post(refresh_pricing))
        .route("/api/health", get(health_check))
        .route("/favicon.ico", get(favicon))
        // `{full_path:path}` matches the EMPTY rest too, so `/assets/` is a real
        // request that reaches the handler and 400s on the containment check.
        // axum's `{*full_path}` requires at least one segment, so the bare path
        // needs its own route or it would fall through to the 404 fallback.
        .route("/assets/", get(assets_root))
        .route("/assets/{*full_path}", get(serve_react_assets))
        // Same `{path:path}` rule, same two routes. `api_route(methods=[…])`
        // claims exactly four verbs, so anything else is the 405 `lib.rs`
        // already answers with FastAPI's `{"detail":"Method Not Allowed"}`.
        .route(
            "/ollama-api/",
            get(ollama_proxy_root)
                .post(ollama_proxy_root)
                .put(ollama_proxy_root)
                .delete(ollama_proxy_root),
        )
        .route(
            "/ollama-api/{*path}",
            get(ollama_proxy_route)
                .post(ollama_proxy_route)
                .put(ollama_proxy_route)
                .delete(ollama_proxy_route),
        )
}

// ── GET /api/pricing ─────────────────────────────────────────────────────────

/// `get_pricing` — `deps.pricing_service.get_pricing()`, re-keyed and 500-wrapped.
///
/// The handler rebuilds the payload key-by-key rather than forwarding the
/// service's dict, and the order it uses (`pricing`, `source`, `timestamp`,
/// `is_stale`) happens to match the service's own — so the byte contract is the
/// same order twice. Note the last key is `pricing_data.get("is_stale", False)`,
/// a `.get` with a default where the other three are subscripts; the service
/// always sets it, so the default is unreachable and is not reproduced as a
/// separate branch.
///
/// The `deps.pricing_service is None` → 503 leg is not ported: the service is
/// constructed in `_lifespan` and its `__init__` only `mkdir`s, so `None` means
/// the process failed to make a directory. There is no corresponding object in
/// the port to be absent, and inventing one to report on would be exactly the
/// fabricated service layer [`health_check`] declines to build.
async fn get_pricing(State(state): State<AppState>) -> Response {
    let app_dir = app_dir_of(&state);
    let package_dir = state.package_dir().to_path_buf();
    let outcome = tokio::task::spawn_blocking(move || {
        let service = PricingService::new(&app_dir);
        // Built eagerly, exactly as `routes/pricing.rs` builds it per request,
        // and it is `crate::pricing::engine` — NEVER `default_engine`, which is
        // a silent 2 % cost error (DIV-056). It feeds only the `source:
        // "default"` leg, which is why the closure is `FnOnce`.
        let engine = match state
            .connect()
            .and_then(|conn| crate::pricing::engine(&conn, &package_dir))
        {
            Ok(engine) => engine,
            Err(err) => return Err(err.to_string()),
        };
        service
            .get_pricing(|| rate_card_payload(&engine))
            .map_err(|raise| raise.message().to_owned())
    })
    .await;

    let payload = match outcome {
        Ok(Ok(data)) => data,
        // `except Exception as e: {"error": f"Failed to get pricing: {str(e)}"}`.
        Ok(Err(message)) => return pricing_failure("Failed to get pricing", &message),
        // A panicking worker has no Python counterpart; it lands in the same
        // 500 shape rather than being swallowed into a well-formed body.
        Err(err) => return pricing_failure("Failed to get pricing", &err.to_string()),
    };

    let mut obj = Map::new();
    obj.insert(
        "pricing".to_owned(),
        payload.get("pricing").cloned().unwrap_or(Value::Null),
    );
    obj.insert(
        "source".to_owned(),
        payload.get("source").cloned().unwrap_or(Value::Null),
    );
    obj.insert(
        "timestamp".to_owned(),
        payload.get("timestamp").cloned().unwrap_or(Value::Null),
    );
    obj.insert(
        "is_stale".to_owned(),
        payload
            .get("is_stale")
            .cloned()
            .unwrap_or(Value::Bool(false)),
    );
    JsonBody::ok(Value::Object(obj)).into_response()
}

// ── POST /api/pricing/refresh ────────────────────────────────────────────────

/// `refresh_pricing` — `force_refresh()`, with the false case as a **500**.
///
/// The failure body is `{"status": "error", "message": …}`, not the `{"error":
/// …}` shape the `except` leg uses; both are 500s, and the two shapes are the
/// reference's, not a simplification.
async fn refresh_pricing(State(state): State<AppState>) -> Response {
    let app_dir = app_dir_of(&state);
    let refreshed =
        tokio::task::spawn_blocking(move || PricingService::new(&app_dir).force_refresh()).await;
    match refreshed {
        Ok(true) => {
            let mut obj = Map::new();
            obj.insert("status".to_owned(), Value::from("success"));
            obj.insert(
                "message".to_owned(),
                Value::from("Pricing updated successfully"),
            );
            JsonBody::ok(Value::Object(obj)).into_response()
        }
        Ok(false) => {
            let mut obj = Map::new();
            obj.insert("status".to_owned(), Value::from("error"));
            obj.insert(
                "message".to_owned(),
                Value::from("Failed to fetch pricing from LiteLLM"),
            );
            JsonBody::with_status(StatusCode::INTERNAL_SERVER_ERROR, Value::Object(obj))
                .into_response()
        }
        Err(err) => pricing_failure("Failed to refresh pricing", &err.to_string()),
    }
}

/// `JSONResponse({"error": f"{prefix}: {str(e)}"}, status_code=500)`.
fn pricing_failure(prefix: &str, message: &str) -> Response {
    let mut obj = Map::new();
    obj.insert(
        "error".to_owned(),
        Value::from(format!("{prefix}: {message}")),
    );
    JsonBody::with_status(StatusCode::INTERNAL_SERVER_ERROR, Value::Object(obj)).into_response()
}

/// `settings.app_dir()` — the directory the store lives in.
fn app_dir_of(state: &AppState) -> PathBuf {
    state
        .store_path()
        .parent()
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf)
}

// ── GET /api/health ──────────────────────────────────────────────────────────

/// `health_check` — five `is not None` probes over `deps`.
///
/// Every one of the five services is constructed in `server.py::_lifespan`
/// inside a `try/except` that only logs, so the map is all-`true` on any install
/// where the constructors succeed — which is all of them here: the four store
/// services open (and create) their own SQLite files under the app dir, and
/// `PricingService` only `mkdir`s a cache directory. None of that machinery
/// exists in the port, so rather than fabricate a service layer to report on,
/// the port reports the *outcome* the reference reports and the case file pins
/// it: the day a constructor starts failing on the harness home, `M-health`
/// goes red and says which one.
///
/// Key order is the dict literal's — `search`, `tags`, `qa`, `bookmarks`,
/// `pricing` — which is not alphabetical and is part of the byte contract.
async fn health_check() -> JsonBody {
    let mut services = Map::new();
    services.insert("search".to_owned(), Value::Bool(true));
    services.insert("tags".to_owned(), Value::Bool(true));
    services.insert("qa".to_owned(), Value::Bool(true));
    services.insert("bookmarks".to_owned(), Value::Bool(true));
    services.insert("pricing".to_owned(), Value::Bool(true));

    let mut obj = Map::new();
    obj.insert("status".to_owned(), Value::from("ok"));
    obj.insert("services".to_owned(), Value::Object(services));
    JsonBody::ok(Value::Object(obj))
}

// ── GET /favicon.ico ─────────────────────────────────────────────────────────

/// `favicon` — the file when it exists, else `JSONResponse({}, status_code=204)`.
///
/// The 204 leg is the interesting one, and it is not the obvious empty response:
/// starlette builds the body `b"{}"` and then `Response.init_headers` suppresses
/// `content-length` for 204/304 while still emitting the `content-type`. Ported
/// as written; the checked-in package tree ships the file, so the harness
/// exercises the `FileResponse` leg and this one stays a reading of the source.
async fn favicon(State(state): State<AppState>) -> Response {
    let path = state.static_dir().join("favicon.ico");
    // `os.path.exists(...)` then `FileResponse(...)` — a race between the two is
    // Python's race too (it raises), so the read is the single source of truth.
    // Since wave-10 item 2c that read goes through `AppState::read_static`, so
    // it answers from the compiled-in bundle unless `STAX_STATIC_DIR` points at
    // a directory; `static_dir()` is still the root the path is built from.
    match state.read_static(&path).await {
        Some(bytes) => file_response(bytes, "image/x-icon"),
        None => {
            let mut response =
                JsonBody::with_status(StatusCode::NO_CONTENT, Value::Object(Map::new()))
                    .into_response();
            // See above: starlette omits `content-length` on a 204 but keeps the
            // media type. axum computes the length from the body, so it comes
            // off explicitly rather than being left to look "helpfully" right.
            response.headers_mut().remove(header::CONTENT_LENGTH);
            response
        }
    }
}

// ── GET /assets/{full_path:path} ─────────────────────────────────────────────

/// `/assets/` with nothing after it — see [`register`].
async fn assets_root() -> Response {
    invalid_path()
}

/// `serve_react_assets` — resolve, contain, serve.
///
/// The containment check is a *string prefix* against `str(assets_dir) + os.sep`
/// after `Path.resolve()` on both sides, so a request resolving exactly to the
/// assets directory itself is "invalid", not "not found". Reproduced, because
/// the two failures carry different status codes.
async fn serve_react_assets(
    State(state): State<AppState>,
    PathParam(full_path): PathParam<String>,
) -> Response {
    let root = state.static_dir().join("react").join("assets");
    let assets_dir = resolve_lexically(&root);
    let file_path = resolve_lexically(&root.join(&full_path));

    let mut prefix = assets_dir.as_os_str().to_string_lossy().into_owned();
    prefix.push(std::path::MAIN_SEPARATOR);
    if !file_path.to_string_lossy().starts_with(&prefix) {
        return invalid_path();
    }
    // The containment check above is unchanged and still runs against
    // `static_dir()` as a *nominal* root — it is pure path math (DIV-401), so
    // it holds with no directory on disk. Only this last step moved: the bytes
    // come from the binary unless `STAX_STATIC_DIR` is set.
    match state.read_static(&file_path).await {
        // `file_path.exists() and file_path.is_file()` — a directory read fails
        // here for the same reason `is_file()` is false there.
        Some(bytes) => {
            let media = guess_media_type(&file_path);
            file_response(bytes, &media)
        }
        None => {
            let mut obj = Map::new();
            obj.insert("error".to_owned(), Value::from("Asset not found"));
            JsonBody::with_status(StatusCode::NOT_FOUND, Value::Object(obj)).into_response()
        }
    }
}

fn invalid_path() -> Response {
    let mut obj = Map::new();
    obj.insert("error".to_owned(), Value::from("Invalid path"));
    JsonBody::with_status(StatusCode::BAD_REQUEST, Value::Object(obj)).into_response()
}

// ── GET|POST|PUT|DELETE /ollama-api/{path:path} ──────────────────────────────

/// `/ollama-api/` with nothing after it — `{path:path}` matches the empty rest,
/// so `path` is `""` and the upstream URL is `http://localhost:11434/api/`.
async fn ollama_proxy_root(request: Request) -> Response {
    forward_to_ollama(String::new(), request).await
}

/// `ollama_proxy` — forward the method, the raw body and (almost) every header.
async fn ollama_proxy_route(PathParam(path): PathParam<String>, request: Request) -> Response {
    forward_to_ollama(path, request).await
}

/// The shared body of both routes.
///
/// Everything from `body = await request.body()` inward is inside Python's
/// `try`, including the body read itself — a client that disconnects mid-upload
/// therefore gets the same 502 as a dead Ollama, which is why the `to_bytes`
/// failure below is not a separate 400.
async fn forward_to_ollama(path: String, request: Request) -> Response {
    let method = request.method().clone();
    let headers = ollama_proxy::forwarded_headers(request.headers());
    let body = match axum::body::to_bytes(request.into_body(), usize::MAX).await {
        Ok(bytes) => bytes,
        Err(_) => return ollama_unavailable(),
    };

    match ollama_proxy::proxy(method.as_str(), &path, &headers, &body).await {
        ProxyOutcome::Json { status, body } => {
            let status = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY);
            JsonBody::with_status(status, body).into_response()
        }
        ProxyOutcome::Stream {
            status,
            headers,
            body,
        } => {
            // `StreamingResponse(stream(), status_code=…, headers=dict(response.headers))`
            // — starlette forwards the upstream headers verbatim and adds no
            // `content-length`, because a streaming body has no known length.
            // Unreachable while 11434 is closed and therefore UNMEASURED: no
            // case row (law 4 bars stream rows outright) and no claim that these
            // bytes match.
            let mut response = Response::builder()
                .status(StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY));
            if let Some(map) = response.headers_mut() {
                for (name, value) in headers {
                    if let (Ok(name), Ok(value)) = (
                        HeaderName::from_bytes(name.as_bytes()),
                        HeaderValue::from_str(&value),
                    ) {
                        map.append(name, value);
                    }
                }
            }
            response
                .body(Body::from(body))
                .unwrap_or_else(|_| ollama_unavailable())
        }
        ProxyOutcome::Unavailable => ollama_unavailable(),
    }
}

/// `except Exception: JSONResponse({"error": "Ollama not available"}, 502)`.
///
/// The whole endpoint on this host, and the reason `M-ollama` is a real row.
fn ollama_unavailable() -> Response {
    let mut obj = Map::new();
    obj.insert("error".to_owned(), Value::from("Ollama not available"));
    JsonBody::with_status(StatusCode::BAD_GATEWAY, Value::Object(obj)).into_response()
}

/// starlette's `FileResponse`, minus the two headers DIV-051 already records.
fn file_response(bytes: Bytes, media_type: &str) -> Response {
    // `Response.init_headers` appends the charset to any `text/*` media type —
    // the same rule `spa::add_text_charset` restores for the `/static` mount.
    let content_type = if media_type.starts_with("text/") && !media_type.contains("charset=") {
        format!("{media_type}; charset=utf-8")
    } else {
        media_type.to_owned()
    };
    let len = bytes.len();
    (
        StatusCode::OK,
        [
            (
                header::CONTENT_TYPE,
                HeaderValue::from_str(&content_type)
                    .unwrap_or_else(|_| HeaderValue::from_static(JSON_CONTENT_TYPE)),
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

/// `mimetypes.guess_type(filename)[0] or "text/plain"`.
///
/// **This is not a lookup table, and the differ is why.** The obvious port — a
/// `match` over the extensions a Vite build emits, seeded from CPython's
/// `_default_mime_types` — was written first and `M-assets-js` failed on it:
/// Python answered `application/javascript` where the built-in map says
/// `text/javascript`. `mimetypes.init()` loads the built-in map and then lets
/// every file in `knownfiles` **override** it, and this host's
/// `/etc/mime.types` maps `js` to `application/javascript`. A hardcoded table is
/// therefore not "CPython's table"; it is CPython's table on a machine with no
/// `/etc/mime.types`, which is not the machine either server runs on.
///
/// So the resolution order is reproduced instead: built-in defaults, then each
/// known file in order, last write winning. Read once and memoised, because
/// `mimetypes` also initialises exactly once per process.
///
/// `mime_guess` is still not the answer even though it ships a table — and it
/// became a direct dependency in wave 10, so the only remaining objection is the
/// real one: its table is a *third* table, and using it here would trade a
/// known-wrong answer for an unknown-wrong one. The `/static` mount does call
/// it, because there the contract is "whatever `ServeDir` did"; here the
/// contract is `mimetypes.guess_type`, and the two disagree on `.js` (DIV-404).
fn guess_media_type(path: &Path) -> String {
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    mime_table()
        .iter()
        .find(|(suffix, _)| *suffix == ext)
        .map_or_else(
            // `guess_type` returning `None` is `FileResponse`'s `"text/plain"`.
            || "text/plain".to_owned(),
            |(_, media)| media.clone(),
        )
}

/// `mimetypes.knownfiles`, in CPython's order — later files override earlier.
const KNOWN_MIME_FILES: [&str; 8] = [
    "/etc/mime.types",
    "/etc/httpd/mime.types",
    "/etc/httpd/conf/mime.types",
    "/etc/apache/mime.types",
    "/etc/apache2/mime.types",
    "/usr/local/etc/httpd/conf/mime.types",
    "/usr/local/lib/netscape/mime.types",
    "/usr/local/etc/mime.types",
];

/// The subset of CPython 3.12's `_default_mime_types` a static bundle can hit.
///
/// Only the entries that are genuinely in the built-in map are listed. Fonts
/// (`.woff2`, `.ttf`, `.otf`) are deliberately ABSENT: they were added to
/// `mimetypes` in 3.13, so on 3.12 they resolve from `/etc/mime.types` or not at
/// all — inventing `font/woff2` here would be a divergence on a host without
/// the system file.
const DEFAULT_MIME_TYPES: [(&str, &str); 15] = [
    ("css", "text/css"),
    ("csv", "text/csv"),
    ("gif", "image/gif"),
    ("htm", "text/html"),
    ("html", "text/html"),
    ("ico", "image/vnd.microsoft.icon"),
    ("jpeg", "image/jpeg"),
    ("jpg", "image/jpeg"),
    ("js", "text/javascript"),
    ("json", "application/json"),
    ("mjs", "text/javascript"),
    ("png", "image/png"),
    ("svg", "image/svg+xml"),
    ("txt", "text/plain"),
    ("wasm", "application/wasm"),
];

/// The resolved suffix → media-type map, built once.
fn mime_table() -> &'static Vec<(String, String)> {
    static TABLE: std::sync::OnceLock<Vec<(String, String)>> = std::sync::OnceLock::new();
    TABLE.get_or_init(|| {
        let mut table: Vec<(String, String)> = DEFAULT_MIME_TYPES
            .iter()
            .map(|(suffix, media)| ((*suffix).to_owned(), (*media).to_owned()))
            .collect();
        for file in KNOWN_MIME_FILES {
            let Ok(text) = std::fs::read_to_string(file) else {
                continue;
            };
            for line in text.lines() {
                // `MimeTypes.readfp`: split on whitespace, then truncate at the
                // first word that STARTS with `#` — a mid-line `#foo` kills the
                // rest of the line, and a bare `#` word does too.
                let mut words = Vec::new();
                for word in line.split_whitespace() {
                    if word.starts_with('#') {
                        break;
                    }
                    words.push(word);
                }
                let Some((media, suffixes)) = words.split_first() else {
                    continue;
                };
                for suffix in suffixes {
                    let key = suffix.to_ascii_lowercase();
                    match table.iter_mut().find(|(existing, _)| *existing == key) {
                        Some(entry) => entry.1 = (*media).to_owned(),
                        None => table.push((key, (*media).to_owned())),
                    }
                }
            }
        }
        table
    })
}

/// `Path.resolve()` without the symlink walk — the same lexical normalisation
/// `routes/projects.rs` uses for its containment check, and for the same reason:
/// the comparison Python makes is on the *string*.
fn resolve_lexically(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Request as HttpRequest;
    use tower::ServiceExt as _;

    /// A router over a scratch app dir: an empty store next to a `cache/` the
    /// tests seed. `package_dir` is the real package tree, because
    /// `crate::pricing::engine` reads `data/models.toml` out of it.
    fn app(tag: &str) -> (Router, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "stax-misc-{tag}-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        ));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        let store = dir.join("store.db");
        drop(rusqlite::Connection::open(&store).expect("store"));
        let package = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../assets");
        let state = AppState::new(store, package, crate::state::Config::default());
        (register(Router::new()).with_state(state), dir)
    }

    async fn send(router: &Router, request: HttpRequest<Body>) -> (StatusCode, String) {
        let response = router
            .clone()
            .oneshot(request)
            .await
            .expect("router answers");
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 22)
            .await
            .expect("body");
        (status, String::from_utf8_lossy(&bytes).into_owned())
    }

    fn verb(method: &str, uri: &str) -> HttpRequest<Body> {
        HttpRequest::builder()
            .method(method)
            .uri(uri)
            .body(Body::empty())
            .expect("request")
    }

    /// The row `M-ollama` flips on. **This test asserts the state of the
    /// machine**: 11434 must be closed for the reference to answer 502, and it
    /// is (`ss -lnt` has no listener; a connect to `127.0.0.1:11434` and to
    /// `[::1]:11434` is refused). If someone starts Ollama, this test goes red
    /// — which is the point. A green tick that depended on a daemon nobody
    /// checked would be worth less than nothing.
    #[tokio::test]
    async fn every_verb_answers_the_502_because_the_port_is_closed() {
        let (router, dir) = app("ollama");
        for (method, uri) in [
            ("GET", "/ollama-api/tags"),
            ("POST", "/ollama-api/generate"),
            ("PUT", "/ollama-api/blobs/sha256:abc"),
            ("DELETE", "/ollama-api/delete"),
            ("GET", "/ollama-api/a/b/c"),
            ("GET", "/ollama-api/"),
        ] {
            let (status, body) = send(&router, verb(method, uri)).await;
            assert_eq!(status, StatusCode::BAD_GATEWAY, "{method} {uri}");
            assert_eq!(
                body, r#"{"error":"Ollama not available"}"#,
                "{method} {uri}"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `api_route(methods=["GET", "POST", "PUT", "DELETE"])` claims four verbs,
    /// so a fifth is starlette's 405 — which `lib.rs` already renders as
    /// FastAPI's JSON. Asserted through the module router, where axum's own
    /// empty 405 is what would show up if the method list were wrong.
    #[tokio::test]
    async fn a_fifth_verb_is_not_claimed_by_the_proxy_route() {
        let (router, dir) = app("ollama-405");
        let (status, _) = send(&router, verb("PATCH", "/ollama-api/tags")).await;
        assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn seed_cache(dir: &Path, body: &str) {
        let cache = dir.join("cache");
        std::fs::create_dir_all(&cache).expect("cache dir");
        std::fs::write(cache.join("pricing.json"), body).expect("seed");
    }

    /// The one `/api/pricing` shape that is deterministic on BOTH sides: a cache
    /// younger than 24 h is served verbatim and nothing fetches, so nothing
    /// writes. This is the branch `rust/PRICING-REFRESH-DIFFER.md` proves
    /// byte-identical.
    #[tokio::test]
    async fn a_fresh_cache_is_served_without_touching_the_network() {
        let (router, dir) = app("pricing-fresh");
        let ts = crate::services::pricing_refresh::now_isoformat();
        seed_cache(
            &dir,
            &format!(r#"{{"timestamp": "{ts}", "source": "litellm", "pricing": {{"m": 1.5}}}}"#),
        );
        let (status, body) = send(&router, verb("GET", "/api/pricing")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body,
            format!(
                r#"{{"pricing":{{"m":1.5}},"source":"cache","timestamp":"{ts}","is_stale":false}}"#
            )
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `cache_data["pricing"]` is a subscript: the missing key is a `KeyError`
    /// whose `str()` is `'pricing'`, and the route interpolates it. Measured
    /// against the reference, not transcribed.
    #[tokio::test]
    async fn a_cache_without_the_pricing_key_is_the_measured_500() {
        let (router, dir) = app("pricing-keyerror");
        let ts = crate::services::pricing_refresh::now_isoformat();
        seed_cache(&dir, &format!(r#"{{"timestamp": "{ts}"}}"#));
        let (status, body) = send(&router, verb("GET", "/api/pricing")).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(body, r#"{"error":"Failed to get pricing: 'pricing'"}"#);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// No cache at all: the fetch is attempted, cannot succeed (no TLS in this
    /// workspace), and the rate card is served as `source: "default"`.
    #[tokio::test]
    async fn an_absent_cache_falls_through_to_the_rate_card() {
        let (router, dir) = app("pricing-default");
        let (status, body) = send(&router, verb("GET", "/api/pricing")).await;
        assert_eq!(status, StatusCode::OK);
        let parsed: Value = serde_json::from_str(&body).expect("json");
        assert_eq!(parsed["source"], Value::from("default"));
        assert_eq!(parsed["is_stale"], Value::Bool(true));
        // The reference, probed under a failed fetch, answered entries in
        // MANIFEST order — the property under test. The first key tracks
        // whichever model heads `[canonical_ids].anthropic`; it became
        // `claude-opus-5` when that model was added ahead of Fable 5.
        let pricing = parsed["pricing"].as_object().expect("object");
        assert_eq!(
            pricing.keys().next().map(String::as_str),
            Some("claude-opus-5")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `force_refresh()` false → **500**, and the failure body is the
    /// `{"status", "message"}` shape, not the `{"error"}` one.
    #[tokio::test]
    async fn refresh_reports_the_fetch_failure_with_a_500() {
        let (router, dir) = app("pricing-refresh");
        let (status, body) = send(
            &router,
            HttpRequest::builder()
                .method("POST")
                .uri("/api/pricing/refresh")
                .body(Body::empty())
                .expect("request"),
        )
        .await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(
            body,
            r#"{"status":"error","message":"Failed to fetch pricing from LiteLLM"}"#
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn health_key_order_is_the_literals_not_the_alphabets() {
        assert_eq!(
            health_check().await.render(),
            r#"{"status":"ok","services":{"search":true,"tags":true,"qa":true,"bookmarks":true,"pricing":true}}"#
        );
    }

    #[test]
    fn traversal_out_of_the_assets_dir_fails_containment() {
        let base = PathBuf::from("/pkg/static/react/assets");
        let escaped = resolve_lexically(&base.join("../../../../etc/passwd"));
        let mut prefix = base.as_os_str().to_string_lossy().into_owned();
        prefix.push('/');
        assert!(!escaped.to_string_lossy().starts_with(&prefix));
    }

    #[test]
    fn the_assets_dir_itself_fails_containment_too() {
        // `str(file_path).startswith(str(assets_dir) + os.sep)` is FALSE for the
        // directory itself — `/assets/` is a 400, not a 404.
        let base = PathBuf::from("/pkg/static/react/assets");
        let mut prefix = base.as_os_str().to_string_lossy().into_owned();
        prefix.push('/');
        assert!(!base.to_string_lossy().starts_with(&prefix));
    }

    #[test]
    fn the_media_type_resolves_the_way_mimetypes_init_does() {
        // Host-dependent by construction — `/etc/mime.types` overrides the
        // built-in map — so the assertions are on the RESOLUTION, not on a
        // literal. `.js` is the one the differ caught: `text/javascript` from
        // the built-ins, `application/javascript` from this host's file.
        let js = guess_media_type(Path::new("a/CostTab-abc.js"));
        assert!(
            js == "text/javascript" || js == "application/javascript",
            "unexpected .js type {js}"
        );
        if std::path::Path::new("/etc/mime.types").is_file() {
            let text = std::fs::read_to_string("/etc/mime.types").expect("readable");
            let from_file = text.lines().any(|line| {
                let mut words = line.split_whitespace();
                words.next() == Some("application/javascript") && words.any(|word| word == "js")
            });
            if from_file {
                assert_eq!(js, "application/javascript");
            }
        }
        // These agree in both tables.
        assert_eq!(guess_media_type(Path::new("a/index.css")), "text/css");
        assert_eq!(guess_media_type(Path::new("a/logo.svg")), "image/svg+xml");
        // No extension: neither source has one, so `FileResponse`'s fallback.
        assert_eq!(guess_media_type(Path::new("a/LICENSE")), "text/plain");
    }

    #[test]
    fn a_comment_word_truncates_the_rest_of_a_mime_types_line() {
        // `readfp` deletes from the first `#`-prefixed WORD onward, so
        // `text/foo  bar # baz` registers `bar` and not `baz`.
        let line = "text/foo  bar # baz";
        let mut words = Vec::new();
        for word in line.split_whitespace() {
            if word.starts_with('#') {
                break;
            }
            words.push(word);
        }
        assert_eq!(words, vec!["text/foo", "bar"]);
    }
}
