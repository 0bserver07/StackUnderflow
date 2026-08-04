//! `CORSMiddleware`, ported — DIV-050.
//!
//! ```python
//! _server_port = config.get("port")
//! app.add_middleware(
//!     CORSMiddleware,
//!     allow_origins=[
//!         f"http://localhost:{_server_port}",
//!         f"http://127.0.0.1:{_server_port}",
//!         "http://localhost:5175",   # vite dev server
//!         "http://127.0.0.1:5175",
//!     ],
//!     allow_credentials=True,
//!     allow_methods=["*"],
//!     allow_headers=["*"],
//! )
//! ```
//!
//! DIV-050 was filed as "not ported, because the differ is same-origin and
//! nothing measures it". The maintainer ruled *port it*, so the first act was
//! to make it measurable: `parity/endpoint-cases.txt` rows can now carry
//! request headers, and the differ compares the five `access-control-*` names
//! plus `vary`. Everything below is transcribed from a probe of the running
//! reference (`fastapi 0.141.1` / `starlette 1.3.1`), not from the docs.
//!
//! # The measured contract
//!
//! Requests **without** an `Origin` header are untouched — the middleware
//! returns before it does anything, so the other 700-odd rows in the matrix
//! cannot move.
//!
//! ## Simple requests (an `Origin`, and not a preflight)
//!
//! | `Origin` | answer |
//! |---|---|
//! | `http://localhost:5175` | the handler's, plus `access-control-allow-credentials: true`, `access-control-allow-origin: http://localhost:5175`, `vary: Origin` |
//! | `http://evil.example` | the handler's, plus `access-control-allow-credentials: true` **and nothing else** |
//!
//! That second row is the surprise and the reason this file exists rather than
//! a `tower_http::cors::CorsLayer` call: `simple_headers` is applied
//! unconditionally to *every* response that carried an `Origin`, and only the
//! origin echo and the `Vary` are gated on the allow-list. `CorsLayer` emits
//! neither header for a rejected origin. The headers are added to whatever the
//! inner stack produced — a `200`, a `404`, a `405`, and (since this layer sits
//! outside [`crate::path_semantics`]) a `307` too; all four are measured rows.
//!
//! ## Preflight (`OPTIONS` **and** an `access-control-request-method`)
//!
//! The preflight never reaches the router. `OPTIONS /api/nope-nothing` and
//! `OPTIONS /api/health/` — an unknown path and a path that would have
//! redirected — both answer `200 OK`, `text/plain; charset=utf-8`, body `OK`.
//! Two rows pin exactly that, because it is the one place a middleware silently
//! outranks routing.
//!
//! | probe | status | body |
//! |---|---|---|
//! | allowed origin, `GET` | `200` | `OK` |
//! | disallowed origin | `400` | `Disallowed CORS origin` |
//! | allowed origin, method `BREW` | `400` | `Disallowed CORS method` |
//! | disallowed origin **and** `BREW` | `400` | `Disallowed CORS origin, method` |
//!
//! and every one of them still carries `vary: Origin`,
//! `access-control-allow-methods: DELETE, GET, HEAD, OPTIONS, PATCH, POST, PUT`,
//! `access-control-max-age: 600` and `access-control-allow-credentials: true`.
//! The `access-control-allow-origin` echo is present on the two rows whose
//! origin passed and absent on the two that failed — a failure list can name
//! `method` while the origin was fine, and the header is set before the failure
//! is rendered.
//!
//! An `OPTIONS` **without** `access-control-request-method` is not a preflight
//! and falls through to the router, which answers `405 allow: GET` with the
//! simple-request headers attached. Rowed.
//!
//! # What is deliberately not reproduced
//!
//! * **The `allow_all_origins` legs.** `allow_origins` is an explicit list, so
//!   `allow_all_origins` is `False` for the whole of this app's life. That kills
//!   `simple_headers["Access-Control-Allow-Origin"] = "*"`, the `has_cookie`
//!   branch (measured: a `Cookie` on a disallowed origin changes nothing) and
//!   the `preflight_headers` `"*"` leg. Writing them would be writing code no
//!   probe can reach.
//! * **`allow_origin_regex`** — not passed.
//! * **`expose_headers`** — not passed, so `Access-Control-Expose-Headers` is
//!   never emitted.
//! * **`allow_headers` as a list.** `["*"]` means `allow_all_headers`, which
//!   *mirrors* `access-control-request-headers` back verbatim and never
//!   computes the sorted safelisted union. Measured: `X-Foo, Content-Type` in,
//!   `X-Foo, Content-Type` out — spacing and casing preserved.
//! * **The webhook receiver.** `cli.py::ingest_webhook_serve_cmd` builds its own
//!   bare `FastAPI()` with no middleware, so [`crate::webhook_receiver_app`]
//!   does not get this layer.

use axum::body::Body;
use axum::extract::Request;
use axum::http::{HeaderName, HeaderValue, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::Response;

use crate::state::Config;

/// starlette's `ALL_METHODS`, in its own declaration order.
///
/// `allow_methods=["*"]` is rewritten to this tuple at construction, and
/// `", ".join(...)` of it is what `access-control-allow-methods` carries. The
/// order is the tuple's, not sorted and not the router's.
const ALL_METHODS: [&str; 7] = ["DELETE", "GET", "HEAD", "OPTIONS", "PATCH", "POST", "PUT"];

/// `", ".join(ALL_METHODS)` — pre-joined because it is a constant.
const ALLOW_METHODS_VALUE: &str = "DELETE, GET, HEAD, OPTIONS, PATCH, POST, PUT";

/// `str(max_age)` with starlette's default `max_age=600`, which `server.py`
/// does not override.
const MAX_AGE_VALUE: &str = "600";

/// The vite dev-server port `server.py` hard-codes alongside the configured one.
const VITE_PORT: u16 = 5175;

/// `Access-Control-Request-Method`, which is what makes an `OPTIONS` a preflight.
const ACCESS_CONTROL_REQUEST_METHOD: HeaderName =
    HeaderName::from_static("access-control-request-method");
/// `Access-Control-Request-Headers` — mirrored back under `allow_headers=["*"]`.
const ACCESS_CONTROL_REQUEST_HEADERS: HeaderName =
    HeaderName::from_static("access-control-request-headers");

/// The allow-list, resolved once from the same setting Python reads.
///
/// **`config.get("port")`, not the listening port.** `server.py` builds the
/// origins from `Settings.port` — the `PORT` env var, then `config.json`, then
/// the `8081` default — while uvicorn is told its port separately. The parity
/// harness runs the reference on `:8097` and its allow-list still names `:8081`,
/// which is precisely the trap a "use the port we bound" implementation would
/// have fallen into and which no same-origin row could have caught.
#[derive(Debug, Clone)]
pub struct CorsPolicy {
    origins: [String; 4],
}

impl CorsPolicy {
    /// Build the four origins from the resolved settings.
    #[must_use]
    pub fn from_config(config: &Config) -> Self {
        let port = config.port;
        Self {
            origins: [
                format!("http://localhost:{port}"),
                format!("http://127.0.0.1:{port}"),
                format!("http://localhost:{VITE_PORT}"),
                format!("http://127.0.0.1:{VITE_PORT}"),
            ],
        }
    }

    /// `is_allowed_origin` — plain membership, because `allow_all_origins` is
    /// `False` and `allow_origin_regex` is `None`.
    ///
    /// Compared as **bytes**. starlette decodes the header latin-1 and compares
    /// Python strings; a latin-1 round trip is byte equality, and going through
    /// `HeaderValue::to_str` instead would reject a non-ASCII origin outright
    /// rather than simply failing to match it.
    #[must_use]
    fn is_allowed(&self, origin: &HeaderValue) -> bool {
        self.origins
            .iter()
            .any(|allowed| allowed.as_bytes() == origin.as_bytes())
    }
}

/// The layer. Outermost, because starlette's user middleware wraps the router.
///
/// Sitting outside [`crate::path_semantics`] is not a detail: a preflight for a
/// path that would 307 answers `200 OK` and never redirects, and the simple
/// headers land on the `307` when it does happen. Both are rows.
pub async fn python_cors(policy: CorsPolicy, req: Request, next: Next) -> Response {
    // `origin = headers.get("origin"); if origin is None: return await self.app(...)`.
    let Some(origin) = req.headers().get(header::ORIGIN).cloned() else {
        return next.run(req).await;
    };

    if req.method() == Method::OPTIONS && req.headers().contains_key(ACCESS_CONTROL_REQUEST_METHOD)
    {
        return preflight_response(&policy, &req, &origin);
    }

    let allowed = policy.is_allowed(&origin);
    let mut res = next.run(req).await;
    // `headers.update(self.simple_headers)` — unconditional. With
    // `allow_all_origins` false the only entry is the credentials flag, and it
    // is added even when the origin is refused.
    res.headers_mut().insert(
        header::ACCESS_CONTROL_ALLOW_CREDENTIALS,
        HeaderValue::from_static("true"),
    );
    if allowed {
        // `allow_explicit_origin`: echo, then `add_vary_header("Origin")`.
        res.headers_mut()
            .insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, origin);
        add_vary_origin(&mut res);
    }
    res
}

/// `MutableHeaders.add_vary_header("Origin")` — append to an existing `Vary`.
///
/// Nothing in this app sets `Vary`, so the join leg is unreachable today; it is
/// written because the reference's is, and because `insert` would silently be
/// right for the wrong reason the day a handler does set one.
fn add_vary_origin(res: &mut Response) {
    let joined = match res.headers().get(header::VARY) {
        Some(existing) => {
            let mut value = existing.as_bytes().to_vec();
            value.extend_from_slice(b", Origin");
            HeaderValue::from_bytes(&value).ok()
        }
        None => Some(HeaderValue::from_static("Origin")),
    };
    if let Some(joined) = joined {
        res.headers_mut().insert(header::VARY, joined);
    }
}

/// `preflight_response` — the whole of it, in the reference's own order.
fn preflight_response(policy: &CorsPolicy, req: &Request, origin: &HeaderValue) -> Response {
    // `preflight_headers`, built once at construction. `Vary` comes first
    // because `preflight_explicit_allow_origin` is true (explicit origins), and
    // the credentials flag comes last. Header ORDER is not what the differ
    // compares, but it is free to be right.
    let mut out: Vec<(HeaderName, HeaderValue)> = vec![
        (header::VARY, HeaderValue::from_static("Origin")),
        (
            header::ACCESS_CONTROL_ALLOW_METHODS,
            HeaderValue::from_static(ALLOW_METHODS_VALUE),
        ),
        (
            header::ACCESS_CONTROL_MAX_AGE,
            HeaderValue::from_static(MAX_AGE_VALUE),
        ),
        (
            header::ACCESS_CONTROL_ALLOW_CREDENTIALS,
            HeaderValue::from_static("true"),
        ),
    ];

    let mut failures: Vec<&str> = Vec::new();
    if policy.is_allowed(origin) {
        out.push((header::ACCESS_CONTROL_ALLOW_ORIGIN, origin.clone()));
    } else {
        failures.push("origin");
    }

    // `if requested_method not in self.allow_methods` — exact, case-sensitive
    // membership in the tuple above. `BREW` fails; so would `get`.
    let requested_method = req
        .headers()
        .get(ACCESS_CONTROL_REQUEST_METHOD)
        .map(HeaderValue::as_bytes)
        .unwrap_or_default();
    if !ALL_METHODS
        .iter()
        .any(|method| method.as_bytes() == requested_method)
    {
        failures.push("method");
    }

    // `allow_all_headers` is true, so the requested headers are mirrored back
    // verbatim — no parsing, no sorting, no safelist union.
    if let Some(requested_headers) = req.headers().get(ACCESS_CONTROL_REQUEST_HEADERS) {
        out.push((
            header::ACCESS_CONTROL_ALLOW_HEADERS,
            requested_headers.clone(),
        ));
    }

    let (status, body) = if failures.is_empty() {
        (StatusCode::OK, "OK".to_owned())
    } else {
        (
            StatusCode::BAD_REQUEST,
            format!("Disallowed CORS {}", failures.join(", ")),
        )
    };
    plain_text(status, &out, body)
}

/// `PlainTextResponse(text, status_code=…, headers=…)`.
///
/// `media_type = "text/plain"`, and `Response.init_headers` appends
/// `; charset=utf-8` to anything starting `text/` — the same rule
/// [`crate::spa`] restores on the static mount.
fn plain_text(status: StatusCode, extra: &[(HeaderName, HeaderValue)], body: String) -> Response {
    let mut res = Response::new(Body::from(body.clone()));
    *res.status_mut() = status;
    for (name, value) in extra {
        res.headers_mut().insert(name.clone(), value.clone());
    }
    res.headers_mut().insert(
        header::CONTENT_LENGTH,
        HeaderValue::from_str(&body.len().to_string())
            .unwrap_or_else(|_| HeaderValue::from_static("0")),
    );
    res.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    res
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> CorsPolicy {
        CorsPolicy::from_config(&Config::default())
    }

    #[test]
    fn the_allow_list_is_built_from_the_settings_port_not_the_bound_one() {
        // `Config::default().port` is 8081, and the harness binds 8096/8097.
        let policy = policy();
        assert_eq!(
            policy.origins,
            [
                "http://localhost:8081",
                "http://127.0.0.1:8081",
                "http://localhost:5175",
                "http://127.0.0.1:5175",
            ]
        );
    }

    #[test]
    fn membership_is_exact() {
        let policy = policy();
        assert!(policy.is_allowed(&HeaderValue::from_static("http://localhost:5175")));
        assert!(policy.is_allowed(&HeaderValue::from_static("http://127.0.0.1:8081")));
        // A trailing slash, a different scheme and a different case are all
        // different strings to Python, so they are different here too.
        assert!(!policy.is_allowed(&HeaderValue::from_static("http://localhost:5175/")));
        assert!(!policy.is_allowed(&HeaderValue::from_static("https://localhost:5175")));
        assert!(!policy.is_allowed(&HeaderValue::from_static("http://LOCALHOST:5175")));
        assert!(!policy.is_allowed(&HeaderValue::from_static("http://evil.example")));
    }

    #[test]
    fn a_configured_port_moves_the_first_two_origins_only() {
        let config = Config {
            port: 9999,
            ..Config::default()
        };
        let policy = CorsPolicy::from_config(&config);
        assert_eq!(policy.origins[0], "http://localhost:9999");
        assert_eq!(policy.origins[1], "http://127.0.0.1:9999");
        assert_eq!(policy.origins[3], "http://127.0.0.1:5175");
    }

    #[test]
    fn vary_is_appended_not_replaced() {
        let mut res = Response::new(Body::empty());
        res.headers_mut()
            .insert(header::VARY, HeaderValue::from_static("Accept-Encoding"));
        add_vary_origin(&mut res);
        assert_eq!(
            res.headers().get(header::VARY).expect("vary"),
            "Accept-Encoding, Origin"
        );
    }

    #[test]
    fn vary_is_set_when_absent() {
        let mut res = Response::new(Body::empty());
        add_vary_origin(&mut res);
        assert_eq!(res.headers().get(header::VARY).expect("vary"), "Origin");
    }

    fn preflight(origin: &str, method: &str, request_headers: Option<&str>) -> Response {
        let mut builder = Request::builder()
            .method(Method::OPTIONS)
            .uri("/api/health")
            .header(header::ORIGIN, origin)
            .header(ACCESS_CONTROL_REQUEST_METHOD, method);
        if let Some(value) = request_headers {
            builder = builder.header(ACCESS_CONTROL_REQUEST_HEADERS, value);
        }
        let req = builder.body(Body::empty()).expect("request");
        let origin = req.headers().get(header::ORIGIN).cloned().expect("origin");
        preflight_response(&policy(), &req, &origin)
    }

    fn header_of(res: &Response, name: HeaderName) -> Option<String> {
        res.headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
    }

    #[test]
    fn an_allowed_preflight_is_two_hundred_with_the_constant_block() {
        let res = preflight("http://localhost:5175", "GET", None);
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(
            header_of(&res, header::ACCESS_CONTROL_ALLOW_METHODS).as_deref(),
            Some("DELETE, GET, HEAD, OPTIONS, PATCH, POST, PUT")
        );
        assert_eq!(
            header_of(&res, header::ACCESS_CONTROL_MAX_AGE).as_deref(),
            Some("600")
        );
        assert_eq!(header_of(&res, header::VARY).as_deref(), Some("Origin"));
        assert_eq!(
            header_of(&res, header::ACCESS_CONTROL_ALLOW_ORIGIN).as_deref(),
            Some("http://localhost:5175")
        );
        assert_eq!(
            header_of(&res, header::CONTENT_TYPE).as_deref(),
            Some("text/plain; charset=utf-8")
        );
    }

    #[test]
    fn a_refused_origin_keeps_the_block_and_drops_only_the_echo() {
        let res = preflight("http://evil.example", "GET", None);
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        assert_eq!(header_of(&res, header::VARY).as_deref(), Some("Origin"));
        assert!(
            header_of(&res, header::ACCESS_CONTROL_ALLOW_ORIGIN).is_none(),
            "the echo is the only header the allow-list gates"
        );
        assert_eq!(
            header_of(&res, header::ACCESS_CONTROL_ALLOW_CREDENTIALS).as_deref(),
            Some("true")
        );
    }

    #[test]
    fn a_refused_method_still_echoes_the_origin() {
        // The measured asymmetry: the echo is set BEFORE the method check, so a
        // `method` failure keeps it.
        let res = preflight("http://localhost:5175", "BREW", None);
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            header_of(&res, header::ACCESS_CONTROL_ALLOW_ORIGIN).as_deref(),
            Some("http://localhost:5175")
        );
    }

    #[test]
    fn requested_headers_are_mirrored_verbatim() {
        let res = preflight("http://localhost:5175", "POST", Some("X-Foo, Content-Type"));
        assert_eq!(
            header_of(&res, header::ACCESS_CONTROL_ALLOW_HEADERS).as_deref(),
            Some("X-Foo, Content-Type"),
            "allow_all_headers mirrors; it does not normalise"
        );
    }

    #[test]
    fn a_lowercase_method_is_not_a_method() {
        // starlette compares against the uppercase tuple with `in`.
        let res = preflight("http://localhost:5175", "get", None);
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    }
}
