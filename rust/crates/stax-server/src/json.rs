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

// ── FastAPI's RequestValidationError — the 422 body ─────────────────────────
//
// A query parameter that will not coerce never reaches the handler: FastAPI's
// dependency solver collects the errors and its installed
// `request_validation_exception_handler` renders
// `JSONResponse({"detail": jsonable_encoder(exc.errors())}, status_code=422)` —
// a *list* of pydantic error objects, not the one-line `{"detail": "<field>"}`
// four route modules shipped before batch C measured it.
//
// This lived in eight route modules under five spellings before the wave-5
// dedup pass, because the claim protocol forbade a batch from editing
// `json.rs`. The five that keyed `msg` off `err.kind` and the three that
// hard-coded the integer message rendered the SAME bytes at every live call
// site — `QueryError::kind` is only ever `int_parsing` or `bool_parsing`, and
// the three int-only copies were reached exclusively from `int_or` / `opt_int`.
// `the_int_only_copies_were_byte_identical_to_the_kind_aware_ones` below is the
// proof that collapsing them moved nothing, and the guard if a `bool_or` call
// is ever added to one of those routes.

/// pydantic's error list for a query parameter that would not coerce.
///
/// The value is the whole `{"detail": [ … ]}` object, so a caller that has to
/// wrap it in its own `JsonBody` (three routes do) gets the same bytes as one
/// that takes [`validation_422`].
///
/// Field order is pydantic's: `type`, `loc`, `msg`, `input`. `loc` is
/// `["query", <field>]` and `input` is the raw string that failed — both
/// measured against fastapi 0.141.1 / pydantic 2.13.4, not transcribed.
#[must_use]
pub fn validation_detail(err: &crate::qs::QueryError) -> Value {
    let mut entry = serde_json::Map::new();
    entry.insert("type".to_owned(), Value::from(err.kind));
    entry.insert(
        "loc".to_owned(),
        Value::Array(vec![Value::from("query"), Value::from(err.field.clone())]),
    );
    entry.insert(
        "msg".to_owned(),
        Value::from(match err.kind {
            "bool_parsing" => "Input should be a valid boolean, unable to interpret input",
            _ => "Input should be a valid integer, unable to parse string as an integer",
        }),
    );
    entry.insert("input".to_owned(), Value::from(err.input.clone()));
    let mut obj = serde_json::Map::new();
    obj.insert(
        "detail".to_owned(),
        Value::Array(vec![Value::Object(entry)]),
    );
    Value::Object(obj)
}

/// [`validation_detail`] at the `422` status — the whole response.
///
/// A `JsonBody`, not an `HttpError`: `HttpError`'s `detail` is a plain string
/// and this one is a list. Same bytes on the wire either way.
#[must_use]
pub fn validation_422(err: &crate::qs::QueryError) -> JsonBody {
    JsonBody::with_status(StatusCode::UNPROCESSABLE_ENTITY, validation_detail(err))
}

/// FastAPI's 422 for an absent *required* query parameter.
///
/// A different pydantic error type from [`validation_detail`]'s — `missing`,
/// with the fixed `"Field required"` message and a null `input`. Measured
/// (`J-content-no-file`), not transcribed.
#[must_use]
pub fn missing_query_param(field: &str) -> Value {
    let mut entry = serde_json::Map::new();
    entry.insert("type".to_owned(), Value::from("missing"));
    entry.insert(
        "loc".to_owned(),
        Value::Array(vec![Value::from("query"), Value::from(field)]),
    );
    entry.insert("msg".to_owned(), Value::from("Field required"));
    entry.insert("input".to_owned(), Value::Null);
    let mut obj = serde_json::Map::new();
    obj.insert(
        "detail".to_owned(),
        Value::Array(vec![Value::Object(entry)]),
    );
    Value::Object(obj)
}

/// The `{"detail": "<field>"}` 422 — **NOT** what FastAPI answers.
///
/// Four call sites (`routes/commands.rs` ×2, `routes/cost.rs`,
/// `routes/budgets.rs`) still spell a coercion failure this way. It is the same
/// latent bug batch C found and fixed in `routes/data.rs` and `/api/stats`: the
/// real body is [`validation_detail`]'s list. Those four endpoints have no case
/// row that sends an uncoercible parameter, so nothing has ever measured them —
/// see the ledger row. Named here so the shape is greppable and so fixing it is
/// one edit; the wave-5 dedup pass was a refactor and did not change the bytes.
#[must_use]
pub fn validation_422_field_only(err: &crate::qs::QueryError) -> HttpError {
    HttpError::new(StatusCode::UNPROCESSABLE_ENTITY, err.field.clone())
}

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

    fn err(field: &str, input: &str, kind: &'static str) -> crate::qs::QueryError {
        crate::qs::QueryError {
            field: field.to_owned(),
            input: input.to_owned(),
            kind,
        }
    }

    /// The bytes the reference answers, for both error kinds. `?page=abc` is
    /// `X-bad-int` / `Q-bad-int`; `?raw_media=maybe` is `J-content-bad-bool`.
    #[test]
    fn the_validation_body_is_pydantics_list_not_a_one_line_detail() {
        assert_eq!(
            validation_422(&err("page", "abc", "int_parsing")).render(),
            r#"{"detail":[{"type":"int_parsing","loc":["query","page"],"msg":"Input should be a valid integer, unable to parse string as an integer","input":"abc"}]}"#
        );
        assert_eq!(
            validation_422(&err("raw_media", "maybe", "bool_parsing")).render(),
            r#"{"detail":[{"type":"bool_parsing","loc":["query","raw_media"],"msg":"Input should be a valid boolean, unable to interpret input","input":"maybe"}]}"#
        );
        assert_eq!(
            validation_422(&err("page", "abc", "int_parsing")).status,
            StatusCode::UNPROCESSABLE_ENTITY
        );
    }

    /// The dedup pass's own gate. `routes/{search,qa,context_replay}.rs` each
    /// carried a copy that ignored `kind` and always wrote the integer message;
    /// those routes only ever build `int_parsing` errors, so the two spellings
    /// agreed on every reachable input. This pins the equality that made
    /// collapsing them a refactor rather than a change.
    #[test]
    fn the_int_only_copies_were_byte_identical_to_the_kind_aware_ones() {
        let int_only = |e: &crate::qs::QueryError| {
            let mut entry = serde_json::Map::new();
            entry.insert("type".to_owned(), Value::from(e.kind));
            entry.insert(
                "loc".to_owned(),
                Value::Array(vec![Value::from("query"), Value::from(e.field.clone())]),
            );
            entry.insert(
                "msg".to_owned(),
                Value::from(
                    "Input should be a valid integer, unable to parse string as an integer",
                ),
            );
            entry.insert("input".to_owned(), Value::from(e.input.clone()));
            let mut obj = serde_json::Map::new();
            obj.insert(
                "detail".to_owned(),
                Value::Array(vec![Value::Object(entry)]),
            );
            Value::Object(obj)
        };
        for (field, input) in [("page", "abc"), ("per_page", ""), ("at", "5.5")] {
            let e = err(field, input, "int_parsing");
            assert_eq!(validation_detail(&e), int_only(&e));
        }
    }

    #[test]
    fn an_absent_required_parameter_is_a_missing_not_a_parse_failure() {
        assert_eq!(
            JsonBody::with_status(
                StatusCode::UNPROCESSABLE_ENTITY,
                missing_query_param("file")
            )
            .render(),
            r#"{"detail":[{"type":"missing","loc":["query","file"],"msg":"Field required","input":null}]}"#
        );
    }

    /// The shape four endpoints still answer, pinned so the dedup pass is
    /// visibly a refactor: it did NOT quietly upgrade them to the list.
    #[test]
    fn the_field_only_422_is_the_unmeasured_shape_and_stays_that_way() {
        assert_eq!(
            validation_422_field_only(&err("timezone_offset", "abc", "int_parsing"))
                .body()
                .render(),
            r#"{"detail":"timezone_offset"}"#
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
