//! Response bodies, written the way starlette writes them.
//!
//! Two rules, both measured rather than assumed.
//!
//! **1. The writer is CPython's, and it is not the CLI's.** starlette's
//! `JSONResponse.render` is
//!
//! ```text
//! json.dumps(content, ensure_ascii=False, allow_nan=False,
//!            indent=None, separators=(",", ":")).encode("utf-8")
//! ```
//!
//! while `cli_helpers/agent_output.py` renders `json.dumps(obj, indent=2)` —
//! `ensure_ascii=True`. Same substrate, opposite flag. So HTTP bodies go through
//! [`stax_memory::pyjson::dumps_http`] and stdout keeps `dumps_pretty`; a
//! project named `café` proves the difference in three bytes versus twelve.
//!
//! `serde_json`'s own writer is not a candidate for either: its float renderer
//! is ryu, a *third* presentation (`1e16` / `1e-5` against CPython's `1e+16` /
//! `1e-05`). §6b's byte-parity requirement is why this module exists at all.
//!
//! **2. `content-type` is exactly `application/json`.** starlette appends
//! `; charset=utf-8` only when the media type starts with `text/`
//! (`Response.init_headers`), so a JSON response carries the bare type. The
//! parity differ compares this header, so getting it "helpfully" right would be
//! wrong.

use axum::body::Body;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde_json::Value;

/// `application/json`, with no charset — see the module docs.
pub const JSON_CONTENT_TYPE: &str = "application/json";

/// A rendered JSON response: starlette's `JSONResponse`.
#[derive(Debug, Clone)]
pub struct JsonBody {
    status: StatusCode,
    value: Value,
}

impl JsonBody {
    /// A `200 OK` JSON body.
    #[must_use]
    pub fn ok(value: Value) -> Self {
        Self {
            status: StatusCode::OK,
            value,
        }
    }

    /// A JSON body with an explicit status — `JSONResponse(payload, status_code=…)`.
    #[must_use]
    pub fn with_status(status: StatusCode, value: Value) -> Self {
        Self { status, value }
    }

    /// The exact bytes this response will put on the wire.
    ///
    /// Public because the parity tests assert on them without a server.
    #[must_use]
    pub fn render(&self) -> String {
        stax_memory::pyjson::dumps_http(&self.value)
    }
}

impl IntoResponse for JsonBody {
    fn into_response(self) -> Response {
        let body = self.render();
        (
            self.status,
            [(
                header::CONTENT_TYPE,
                HeaderValue::from_static(JSON_CONTENT_TYPE),
            )],
            Body::from(body),
        )
            .into_response()
    }
}

/// FastAPI's `HTTPException` — a status plus a `detail`.
///
/// FastAPI's installed `http_exception_handler` renders it as
/// `JSONResponse({"detail": exc.detail}, status_code=exc.status_code)`, so the
/// body is a one-key object and the `detail` may be any JSON value (every
/// raise in the ported routes passes a string).
#[derive(Debug, Clone)]
pub struct HttpError {
    status: StatusCode,
    detail: Value,
}

impl HttpError {
    /// `raise HTTPException(status_code=status, detail=detail)`.
    #[must_use]
    pub fn new(status: StatusCode, detail: impl Into<String>) -> Self {
        Self {
            status,
            detail: Value::String(detail.into()),
        }
    }

    /// `HTTPException(status_code=400, detail=…)`.
    #[must_use]
    pub fn bad_request(detail: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, detail)
    }

    /// `HTTPException(status_code=404, detail=…)`.
    #[must_use]
    pub fn not_found(detail: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, detail)
    }

    /// The rendered body, for tests.
    #[must_use]
    pub fn body(&self) -> JsonBody {
        let mut obj = serde_json::Map::new();
        obj.insert("detail".to_owned(), self.detail.clone());
        JsonBody::with_status(self.status, Value::Object(obj))
    }
}

impl IntoResponse for HttpError {
    fn into_response(self) -> Response {
        self.body().into_response()
    }
}

/// Either a rendered body or an `HTTPException` — the return type of a ported
/// handler that can `raise`.
pub type HandlerResult = Result<JsonBody, HttpError>;

/// FastAPI's fallback for a path no route claims.
///
/// Starlette's default 404 is `PlainTextResponse("Not Found")`, but FastAPI
/// replaces it: `Router.not_found` raises `HTTPException(404)` and the installed
/// handler renders `{"detail":"Not Found"}` as JSON. axum's own fallback is an
/// *empty* 404 with no `content-type` at all, so without this the differ finds a
/// three-way divergence (status agrees, header absent, body empty) on every
/// unknown path — which is exactly what it did find.
#[must_use]
pub fn not_found() -> JsonBody {
    HttpError::new(StatusCode::NOT_FOUND, "Not Found").body()
}

/// FastAPI's fallback for a known path and an unclaimed method.
///
/// Same mechanism, different detail string: starlette's router raises
/// `HTTPException(405)` and the same handler renders it.
#[must_use]
pub fn method_not_allowed() -> JsonBody {
    HttpError::new(StatusCode::METHOD_NOT_ALLOWED, "Method Not Allowed").body()
}

/// Turn a panicking-or-failing blocking task into the 500 Python would produce.
///
/// The ported handlers that wrap their body in `try/except Exception` and return
/// `JSONResponse({"error": …}, 500)` do their own catching; this is for the
/// `spawn_blocking` join itself, which has no Python counterpart because
/// `run_in_threadpool` propagates. A join failure means the worker panicked,
/// which is a bug, not a divergence — so it surfaces as a 500 with the panic
/// text rather than being swallowed.
#[must_use]
pub fn join_failure(err: &tokio::task::JoinError) -> HttpError {
    HttpError::new(
        StatusCode::INTERNAL_SERVER_ERROR,
        format!("worker task failed: {err}"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bodies_render_with_starlettes_flags() {
        let body = JsonBody::ok(serde_json::json!({"name": "café", "n": 1.0}));
        // ensure_ascii=False, compact separators, CPython float repr.
        assert_eq!(body.render(), "{\"name\":\"café\",\"n\":1.0}");
    }

    #[test]
    fn http_exception_is_a_single_detail_key() {
        let err = HttpError::bad_request("No project selected");
        assert_eq!(err.body().render(), r#"{"detail":"No project selected"}"#);
        assert_eq!(err.body().status, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn content_type_carries_no_charset() {
        let response = JsonBody::ok(serde_json::json!({})).into_response();
        assert_eq!(
            response
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some("application/json")
        );
    }

    #[test]
    fn the_fallbacks_are_fastapis_json_not_starlettes_plain_text() {
        assert_eq!(not_found().render(), r#"{"detail":"Not Found"}"#);
        assert_eq!(
            method_not_allowed().render(),
            r#"{"detail":"Method Not Allowed"}"#
        );
    }

    #[test]
    fn key_order_is_the_payloads_not_the_alphabets() {
        let mut obj = serde_json::Map::new();
        obj.insert("total_count".to_owned(), Value::from(3));
        obj.insert("limit".to_owned(), Value::from(500));
        obj.insert("offset".to_owned(), Value::from(0));
        assert_eq!(
            JsonBody::ok(Value::Object(obj)).render(),
            r#"{"total_count":3,"limit":500,"offset":0}"#
        );
    }
}
