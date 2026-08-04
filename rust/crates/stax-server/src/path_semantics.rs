//! The path, before routing — DIV-168 (`%2F`) and DIV-133 / DIV-361 (the 307s).
//!
//! [`crate::method_semantics`] is the same shape one layer in: measure the
//! reference first, then put the rule *outside* the router, because both of
//! these have to happen before the matcher runs. This file is the other half of
//! that pair and it closes three ledger rows the router-shaped ones could not.
//!
//! # 1. uvicorn unquotes the path; axum does not — DIV-168
//!
//! ```python
//! # uvicorn/protocols/http/httptools_impl.py
//! raw_path, _, query_string = url.partition(b"?")
//! path = raw_path.decode("ascii")
//! if "%" in path:
//!     path = urllib.parse.unquote(path)
//! ```
//!
//! starlette therefore matches a **decoded** path, and axum matches the raw
//! one. Measured on the reference against the port, before a line here:
//!
//! | request | reference | port, before |
//! |---|---|---|
//! | `/api/static-analysis/session/a%2Fb` | `404` | `200`, `session_id: "a/b"` |
//! | `/api/interaction/a%2Fb` | `404` | `400` |
//! | `/api/context-replay/a%2Fb` | `404` | `200` |
//! | `/api/pl%61n` | `200`, the plan payload | `404` |
//! | `/api/static-analysis/session/a%20b` | `200`, `"a b"` | `200`, `"a b"` — already identical |
//! | `/api/static-analysis/session/a%2520b` | `200`, `"a%20b"` | `200`, `"a%20b"` — already identical |
//!
//! The last three rows are the reason this is a decode-and-re-encode rather
//! than a decode: `axum::extract::Path` percent-decodes what the matcher
//! captured, so handing it an already-decoded path would decode `%2520` twice
//! and turn a green row red. [`route_path`] decodes exactly as uvicorn does,
//! and [`encode_for_routing`] re-encodes everything except `/` — so a decoded
//! `%2F` stays a structural slash (which is the whole divergence) while every
//! other escape is put back byte-for-byte for `Path` to undo. `%61` → `a` is
//! unreserved and survives both directions, which is why `/api/pl%61n` starts
//! working without anything else moving.
//!
//! DIV-168 named three endpoints. `/api/pl%61n` is a fourth class it did not:
//! *any* escape of an unreserved character re-routes on the reference, not just
//! `%2F`. It was found by writing the row before reading the code, which is the
//! campaign's own rule and the fourth time it has paid.
//!
//! # 2. starlette redirects a trailing slash — DIV-133, and DIV-361 with it
//!
//! ```python
//! # starlette/routing.py Router.app, after every route has failed to match
//! if scope["type"] == "http" and self.redirect_slashes and route_path != "/":
//!     redirect_scope = dict(scope)
//!     if route_path.endswith("/"):
//!         redirect_scope["path"] = redirect_scope["path"].rstrip("/")
//!     else:
//!         redirect_scope["path"] = redirect_scope["path"] + "/"
//!     for route in self.routes:
//!         if route.matches(redirect_scope)[0] != Match.NONE:
//!             return RedirectResponse(url=str(URL(scope=redirect_scope)))
//! ```
//!
//! Measured, with the whole response and not just the status:
//!
//! ```text
//! GET /api/plan/          307  location: http://127.0.0.1:8097/api/plan   content-length: 0, no content-type
//! GET /api/projects/?limit=2   307  location: …/api/projects?limit=2
//! PUT /api/plan/          307   — the method does not matter
//! HEAD /api/plan/         307
//! GET /api/plan//         307  location: …/api/plan        — `rstrip` takes all of them
//! GET /settings/          307  location: …/settings
//! GET /live/              307
//! GET /project            307  location: …/project/        — the OTHER direction
//! GET /static             307  location: …/static/         — DIV-361, on every method
//! GET /nope/              404  — the trimmed path matches nothing
//! GET //                  404  — `"//".rstrip("/")` is `""`
//! GET /static/            404  — the mount matches, `StaticFiles` answers
//! ```
//!
//! Four facts a "just normalise the path" fix would have got wrong, and all
//! four are rows:
//!
//! 1. It is a **`307`**, not a rewrite. `NormalizePathLayer` answers `200` and
//!    would have been a silent divergence on every endpoint at once — DIV-133
//!    says so in writing and it is why this took a fallback instead.
//! 2. The redirect fires **only when nothing matched at all** — after the
//!    method check, so a `405` stays a `405`.
//! 3. It goes **both ways**. `/project` → `/project/` is the append direction
//!    and `/static` → `/static/` is DIV-361 wearing a `Mount` instead of a
//!    route; one rule closes both, which is what DIV-361's entry predicted.
//! 4. The `location` is an **absolute** URL built from the `Host` header, with
//!    the query string appended raw and the whole thing put through
//!    `urllib.parse.quote(url, safe=":/%#?=@[]!$&'()*+,;")`.
//!
//! ## How "did anything match?" is answered without a second route table
//!
//! The same trick [`crate::method_semantics`] uses for `Allow`: ask the router.
//! The app's own `fallback` — the one that renders `{"detail":"Not Found"}` —
//! stamps [`Unmatched`] into the response extensions, so "no route matched" is
//! a fact the router reports rather than one this layer re-derives. A request
//! that comes back carrying it is re-probed **once**, with the toggled path and
//! a method no route registers, and the marker on *that* answer decides the
//! redirect. Matched requests pay nothing.
//!
//! The single exception is the mount root, and it is a genuine difference
//! between the two routers rather than a shortcut: starlette compiles
//! `Mount("/static")` to `^/static/(?P<path>.*)$`, which does **not** match
//! `/static`, while `axum::Router::nest_service` registers `/static` *and*
//! `/static/{*rest}`. So `/static` is matched here and unmatched there, the
//! marker can never appear, and [`is_mount_root`] states the difference in one
//! place instead of letting it hide.

use axum::Router;
use axum::body::Body;
use axum::extract::Request;
use axum::http::uri::PathAndQuery;
use axum::http::{HeaderValue, Method, StatusCode, Uri, header};
use axum::middleware::Next;
use axum::response::Response;
use tower::ServiceExt as _;

/// Stamped on the app fallback's response: **no route matched this path**.
///
/// A response extension rather than a status test, because a `404` is also a
/// perfectly ordinary handler answer (`/api/session/{id}` for an unknown id,
/// the static mount for a missing file) and redirecting on those would invent
/// a `307` the reference never sends.
#[derive(Debug, Clone, Copy)]
pub struct Unmatched;

/// `app.mount("/static", …)` — the one path axum matches and starlette does not.
///
/// Passed to the layer rather than read from it: the dashboard app mounts it and
/// [`crate::webhook_receiver_app`] does not, and a receiver that answered `307`
/// to `/static` would be inventing a mount it has never had.
pub const STATIC_MOUNT: &str = "/static";

/// A method no route registers, so axum's matcher answers "known path" (`405`)
/// or "unknown path" (the `404` fallback, which carries [`Unmatched`]).
const PROBE_METHOD: Method = Method::TRACE;

/// `urllib.parse.quote`'s always-safe set, minus the ones spelled in `safe`.
///
/// CPython: `ascii_letters + digits + "_.-~"`.
const ALWAYS_SAFE: &[u8] = b"_.-~";

/// The `safe=` argument `RedirectResponse` passes: `":/%#?=@[]!$&'()*+,;"`.
const REDIRECT_SAFE: &[u8] = b":/%#?=@[]!$&'()*+,;";

/// Is this the bare mount root — matched by axum, unmatched by starlette?
#[must_use]
fn is_mount_root(mount_root: Option<&str>, path: &str) -> bool {
    mount_root == Some(path)
}

/// uvicorn's `urllib.parse.unquote(path)`, with `errors="replace"`.
///
/// Only the `%XX` pairs are decoded — `+` is **not** a space here, because
/// uvicorn calls `unquote` and not `unquote_plus` (`crate::qs` calls the other
/// one, for the query string, and the asymmetry is the reference's).
/// A malformed escape is left literal, and a byte run that is not valid UTF-8
/// becomes `U+FFFD`, which is what `errors="replace"` does.
#[must_use]
pub fn route_path(raw: &str) -> String {
    if !raw.contains('%') {
        return raw.to_owned();
    }
    let bytes = raw.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && index + 2 < bytes.len()
            && let (Some(high), Some(low)) = (hex(bytes[index + 1]), hex(bytes[index + 2]))
        {
            out.push(high << 4 | low);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Put the decoded path back on the wire with `/` — and only `/` — structural.
///
/// The round trip is the point: `axum::extract::Path` decodes what it captured,
/// so every byte that was an escape must go back to being one, or a handler
/// that already agreed with the reference would start disagreeing. Encoding
/// *more* than the reference's own spelling is harmless (the decoder undoes it
/// either way); encoding less is not.
#[must_use]
fn encode_for_routing(decoded: &str) -> String {
    let mut out = String::with_capacity(decoded.len());
    for byte in decoded.bytes() {
        if byte.is_ascii_alphanumeric() || ALWAYS_SAFE.contains(&byte) || byte == b'/' {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// `urllib.parse.quote(text, safe=":/%#?=@[]!$&'()*+,;")`.
#[must_use]
fn py_quote(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric()
            || ALWAYS_SAFE.contains(&byte)
            || REDIRECT_SAFE.contains(&byte)
        {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// The path starlette would have retried: `rstrip("/")` or `+ "/"`.
///
/// `rstrip` takes **every** trailing slash, which is why `/api/plan//` lands on
/// `/api/plan` and `//` lands on the empty string — a path no route can match,
/// and the reason `//` is a `404` on both sides rather than a redirect to `/`.
#[must_use]
fn toggle_slash(route_path: &str) -> String {
    if route_path.ends_with('/') {
        route_path.trim_end_matches('/').to_owned()
    } else {
        format!("{route_path}/")
    }
}

/// Does any route claim this path? Asked of the router, never re-derived.
async fn is_routed(probe: &Router, mount_root: Option<&str>, path: &str) -> bool {
    if is_mount_root(mount_root, path) {
        // starlette's `Mount` regex needs the slash; axum's `nest_service` does
        // not. This is the difference, stated once — DIV-361.
        return false;
    }
    let Ok(uri) = path.parse::<Uri>() else {
        return false;
    };
    let Ok(request) = Request::builder()
        .method(PROBE_METHOD)
        .uri(uri)
        .body(Body::empty())
    else {
        return false;
    };
    match probe.clone().oneshot(request).await {
        Ok(response) => response.extensions().get::<Unmatched>().is_none(),
        // `Router`'s error is `Infallible`; a failure here would mean the probe
        // itself broke, and answering "not routed" keeps the 404 the request
        // was already going to get.
        Err(_) => false,
    }
}

/// `RedirectResponse(url=str(URL(scope=redirect_scope)))` — `307`, empty, no
/// `content-type`.
fn redirect_to(req_headers: &axum::http::HeaderMap, path: &str, query: Option<&str>) -> Response {
    // `URL(scope=...)`: scheme from the scope (always `http` here — this server
    // does not terminate TLS), authority from the `Host` header verbatim.
    // starlette falls back to the ASGI `server` tuple and then to a bare path
    // when there is no `Host`; HTTP/1.1 requires one, so the bare-path leg is
    // the honest fallback rather than an invented authority.
    let mut url = match req_headers.get(header::HOST) {
        Some(host) => format!("http://{}{path}", String::from_utf8_lossy(host.as_bytes())),
        None => path.to_owned(),
    };
    if let Some(query) = query
        && !query.is_empty()
    {
        // Appended RAW — uvicorn never decodes the query string, and `quote`'s
        // safe set keeps `?`, `=`, `&` and `%` intact.
        url.push('?');
        url.push_str(query);
    }
    let location = py_quote(&url);

    let mut res = Response::new(Body::empty());
    *res.status_mut() = StatusCode::TEMPORARY_REDIRECT;
    if let Ok(value) = HeaderValue::from_str(&location) {
        res.headers_mut().insert(header::LOCATION, value);
    }
    res.headers_mut()
        .insert(header::CONTENT_LENGTH, HeaderValue::from_static("0"));
    res
}

/// The layer. Outside the router, inside [`crate::cors`].
///
/// `probe` is a clone of the very same router the request is about to run
/// through — cheap (axum's `Router` is `Arc`-backed) and, more to the point,
/// incapable of drifting from it.
pub async fn python_path_semantics(
    probe: Router,
    mount_root: Option<&'static str>,
    mut req: Request,
    next: Next,
) -> Response {
    let raw_path = req.uri().path().to_owned();
    let query = req.uri().query().map(str::to_owned);

    // Step 1 — route on the path uvicorn hands starlette. DIV-168.
    let decoded = route_path(&raw_path);
    let encoded = encode_for_routing(&decoded);
    if encoded != raw_path {
        let target = match query.as_deref() {
            Some(query) => format!("{encoded}?{query}"),
            None => encoded.clone(),
        };
        if let Ok(path_and_query) = target.parse::<PathAndQuery>() {
            let mut parts = req.uri().clone().into_parts();
            parts.path_and_query = Some(path_and_query);
            if let Ok(uri) = Uri::from_parts(parts) {
                *req.uri_mut() = uri;
            }
        }
    }

    // Step 2 — the mount root. Matched here, unmatched there, so the marker can
    // never fire: the difference is answered before the request runs. Every
    // method, which is what `!AL-static-root-{get,head,put}` measure.
    if is_mount_root(mount_root, &decoded) {
        return redirect_to(req.headers(), &toggle_slash(&decoded), query.as_deref());
    }

    let headers = req.headers().clone();
    let res = next.run(req).await;

    // Step 3 — the redirect, only on a path the router itself disowned.
    if res.extensions().get::<Unmatched>().is_some() && decoded != "/" {
        let toggled = toggle_slash(&decoded);
        if !toggled.is_empty() && is_routed(&probe, mount_root, &toggled).await {
            return redirect_to(&headers, &toggled, query.as_deref());
        }
    }
    res
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unquote_is_uvicorns_and_not_the_query_strings() {
        assert_eq!(route_path("/api/plan"), "/api/plan");
        assert_eq!(route_path("/api/pl%61n"), "/api/plan");
        assert_eq!(route_path("/a%2Fb"), "/a/b");
        assert_eq!(route_path("/a%2fb"), "/a/b");
        assert_eq!(route_path("/a%20b"), "/a b");
        assert_eq!(route_path("/a%2520b"), "/a%20b");
        // `+` is a space in a QUERY string and a plus in a PATH; uvicorn calls
        // `unquote`, `crate::qs` calls `unquote_plus`. Measured on the
        // reference: `/api/static-analysis/session/a+b` answers `"a+b"`.
        assert_eq!(route_path("/a+b"), "/a+b");
        // A malformed escape stays literal, exactly as `unquote` leaves it.
        assert_eq!(route_path("/a%zzb"), "/a%zzb");
        assert_eq!(route_path("/a%2"), "/a%2");
        // `errors="replace"`.
        assert_eq!(route_path("/%FF"), "/\u{FFFD}");
        assert_eq!(route_path("/%C3%A9"), "/é");
    }

    #[test]
    fn the_re_encode_keeps_only_the_slash_structural() {
        // The `%2F` fix: a decoded slash is left alone, so the matcher sees the
        // extra segment the reference's `[^/]+` convertor refuses to span.
        assert_eq!(encode_for_routing("/a/b"), "/a/b");
        // Everything else goes back to being an escape for `Path` to undo.
        assert_eq!(encode_for_routing("/a b"), "/a%20b");
        assert_eq!(encode_for_routing("/a%20b"), "/a%2520b");
        assert_eq!(encode_for_routing("/a+b"), "/a%2Bb");
        assert_eq!(encode_for_routing("/é"), "/%C3%A9");
        // Unreserved characters survive the round trip untouched, which is what
        // makes `/api/pl%61n` route.
        assert_eq!(encode_for_routing("/api/plan"), "/api/plan");
        assert_eq!(encode_for_routing("/a-b_c.d~e"), "/a-b_c.d~e");
    }

    #[test]
    fn the_round_trip_is_idempotent_on_an_already_plain_path() {
        for path in ["/", "/api/plan", "/project/a/b", "/static/react/index.html"] {
            assert_eq!(encode_for_routing(&route_path(path)), path, "{path}");
        }
    }

    #[test]
    fn quote_uses_the_redirect_safe_set() {
        assert_eq!(py_quote("http://h/api/plan"), "http://h/api/plan");
        assert_eq!(
            py_quote("http://h/api/plan?a=b&c=d%20e"),
            "http://h/api/plan?a=b&c=d%20e"
        );
        // A space is not safe; `%` is, which is why an already-encoded query
        // survives unchanged.
        assert_eq!(py_quote("/a b"), "/a%20b");
        assert_eq!(py_quote("/é"), "/%C3%A9");
        // The safe set is verbatim from `RedirectResponse`.
        assert_eq!(py_quote("/:#[]!$&'()*+,;@="), "/:#[]!$&'()*+,;@=");
    }

    #[test]
    fn the_toggle_strips_every_trailing_slash_and_appends_exactly_one() {
        assert_eq!(toggle_slash("/api/plan/"), "/api/plan");
        assert_eq!(toggle_slash("/api/plan//"), "/api/plan");
        assert_eq!(toggle_slash("/api/plan"), "/api/plan/");
        assert_eq!(toggle_slash("/project"), "/project/");
        assert_eq!(toggle_slash("/static"), "/static/");
        // `"//".rstrip("/")` is `""` — nothing matches it, so `//` is a 404 on
        // both sides and the caller checks for exactly this.
        assert_eq!(toggle_slash("//"), "");
    }

    #[test]
    fn only_the_bare_mount_root_is_the_exception() {
        let mount = Some(STATIC_MOUNT);
        assert!(is_mount_root(mount, "/static"));
        assert!(!is_mount_root(mount, "/static/"));
        assert!(!is_mount_root(mount, "/static/react/index.html"));
        assert!(!is_mount_root(mount, "/statics"));
        // The webhook receiver has no mount, so `/static` there is an ordinary
        // unmatched path and must not grow a redirect it never had.
        assert!(!is_mount_root(None, "/static"));
    }

    #[tokio::test]
    async fn the_measured_redirect_shape_is_reproduced() {
        let headers = {
            let mut map = axum::http::HeaderMap::new();
            map.insert(header::HOST, HeaderValue::from_static("127.0.0.1:8097"));
            map
        };
        let res = redirect_to(&headers, "/api/plan", None);
        assert_eq!(res.status(), StatusCode::TEMPORARY_REDIRECT);
        assert_eq!(
            res.headers().get(header::LOCATION).expect("location"),
            "http://127.0.0.1:8097/api/plan"
        );
        assert_eq!(res.headers().get(header::CONTENT_LENGTH).expect("len"), "0");
        assert!(
            res.headers().get(header::CONTENT_TYPE).is_none(),
            "the reference sends no content-type on a redirect"
        );

        // The query rides along raw.
        let res = redirect_to(&headers, "/api/projects", Some("limit=2"));
        assert_eq!(
            res.headers().get(header::LOCATION).expect("location"),
            "http://127.0.0.1:8097/api/projects?limit=2"
        );
        let res = redirect_to(&headers, "/api/plan", Some("a=b&c=d%20e"));
        assert_eq!(
            res.headers().get(header::LOCATION).expect("location"),
            "http://127.0.0.1:8097/api/plan?a=b&c=d%20e"
        );
    }
}
