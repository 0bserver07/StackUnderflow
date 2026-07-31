//! `routes/misc.py` — 6 endpoints, wave 5 (batch A).
//!
//! | Item | Method | FastAPI path | axum path | State |
//! |---|---|---|---|---|
//! | `RS-5-079` | `GET`  | `/api/pricing`             | `/api/pricing`          | **open** — DIV-065 |
//! | `RS-5-080` | `POST` | `/api/pricing/refresh`     | `/api/pricing/refresh`  | **open** — DIV-065 |
//! | `RS-5-081` | `GET`  | `/api/health`              | `/api/health`           | ported |
//! | `RS-5-082` | `GET`  | `/favicon.ico`             | `/favicon.ico`          | ported |
//! | `RS-5-083` | `GET`  | `/assets/{full_path:path}` | `/assets/{*full_path}`  | ported |
//! | `RS-5-084` | `GET`… | `/ollama-api/{path:path}`  | `/ollama-api/{*path}`   | **open** — DIV-066 |
//!
//! # What is ported, and what is a network call wearing a route's clothes
//!
//! Three of these six read the filesystem and answer. The other three do not
//! answer from local state at all:
//!
//! * `/api/pricing` calls `PricingService.get_pricing()`, which on a cache older
//!   than 24 h issues a blocking `urlopen` to LiteLLM on GitHub and **writes the
//!   fetched payload back to `$STACKUNDERFLOW_HOME/cache/pricing.json`**. Its
//!   body is therefore a function of the network *and* of which server asked
//!   first — the second one diffed would read the cache the first just wrote.
//!   No ordering makes that a byte comparison. DIV-065.
//! * `/api/pricing/refresh` is the same fetch, unconditionally, plus the write.
//!   DIV-065.
//! * `/ollama-api/{path}` proxies to `http://localhost:11434`. DIV-066.
//!
//! All three are filed rather than stubbed-with-a-guess. `/ollama-api` carries a
//! `!`-prefixed row in `parity/endpoint-cases.txt` so the differ reports it every
//! run; the two pricing routes deliberately do **not**, and that exception was
//! earned rather than assumed. They went in as `!` rows first, and the run
//! proved the problem: a `!` row still ISSUES the request — the marker only
//! softens the verdict — so Python fetched LiteLLM and wrote
//! `cache/pricing.json` into the shared home, which turned all five
//! `GET /api/pricing/doctor` rows from a deterministic "no overlay on disk"
//! payload into a live elapsed-time `age_days` float. Five clean cases became
//! five divergences on one field, from a case file edit, with no code change.
//! An endpoint whose side effect is another endpoint's input cannot share a home
//! with it, so this one is tracked in the ledger and here instead.

use std::path::{Path, PathBuf};

use axum::Router;
use axum::body::Body;
use axum::extract::{Path as PathParam, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde_json::{Map, Value};

use crate::json::{JSON_CONTENT_TYPE, JsonBody};
use crate::state::AppState;

/// Mount this module's endpoints onto `router`.
pub fn register(router: Router<AppState>) -> Router<AppState> {
    router
        .route("/api/health", get(health_check))
        .route("/favicon.ico", get(favicon))
        // `{full_path:path}` matches the EMPTY rest too, so `/assets/` is a real
        // request that reaches the handler and 400s on the containment check.
        // axum's `{*full_path}` requires at least one segment, so the bare path
        // needs its own route or it would fall through to the 404 fallback.
        .route("/assets/", get(assets_root))
        .route("/assets/{*full_path}", get(serve_react_assets))
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
    match tokio::fs::read(&path).await {
        Ok(bytes) => file_response(bytes, "image/x-icon"),
        Err(_) => {
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
    match tokio::fs::read(&file_path).await {
        // `file_path.exists() and file_path.is_file()` — a directory read fails
        // here for the same reason `is_file()` is false there.
        Ok(bytes) => {
            let media = guess_media_type(&file_path);
            file_response(bytes, &media)
        }
        Err(_) => {
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

/// starlette's `FileResponse`, minus the two headers DIV-051 already records.
fn file_response(bytes: Vec<u8>, media_type: &str) -> Response {
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
/// `mime_guess` is still not the answer even though it ships a table: it is not
/// a direct dependency, and its table is a *third* one — it would trade a
/// known-wrong answer for an unknown-wrong one.
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
