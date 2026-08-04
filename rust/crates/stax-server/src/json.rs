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

/// pydantic's 422 for a parameter that COERCED and then failed a bound.
///
/// A third shape: `type`, `loc`, `msg`, `input`, and then a `ctx` object
/// carrying the bound itself — `{"ge": 1}` / `{"le": 500}` — appended after
/// `input`. `input` is the RAW string, not the parsed integer. Measured
/// against fastapi 0.141.1 / pydantic 2.13.4, not transcribed.
///
/// This is not reachable through [`validation_detail`], whose `QueryError`
/// has no bound to report: a `QueryError` is a *coercion* failure. Lived
/// privately in `routes/agent_teams.rs` (the only `Query(…, ge=…, le=…)` in
/// the reference) until `/api/search` and `/api/qa` declared a floor too —
/// promoted rather than copied, on the wave-5 dedup precedent.
#[must_use]
pub fn bound_422(
    field: &str,
    kind: &str,
    msg: &str,
    raw_input: &str,
    ctx_key: &str,
    ctx_value: i64,
) -> JsonBody {
    let mut ctx = serde_json::Map::new();
    ctx.insert(ctx_key.to_owned(), Value::from(ctx_value));
    let mut entry = serde_json::Map::new();
    entry.insert("type".to_owned(), Value::from(kind));
    entry.insert(
        "loc".to_owned(),
        Value::Array(vec![Value::from("query"), Value::from(field)]),
    );
    entry.insert("msg".to_owned(), Value::from(msg));
    entry.insert("input".to_owned(), Value::from(raw_input));
    entry.insert("ctx".to_owned(), Value::Object(ctx));
    let mut obj = serde_json::Map::new();
    obj.insert(
        "detail".to_owned(),
        Value::Array(vec![Value::Object(entry)]),
    );
    JsonBody::with_status(StatusCode::UNPROCESSABLE_ENTITY, Value::Object(obj))
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

// ── the `dict`-annotated body parameter — DIV-367 ────────────────────────────
//
// `async def set_project(data: dict[str, str])` is NOT "give me the parsed
// body". It is a validator, and it runs BEFORE the handler exists:
//
//   fastapi/routing.py   body_bytes = await request.body()
//                        if body_bytes: body = json.loads(...)   # else None
//                        except json.JSONDecodeError: -> json_invalid
//   fastapi/_compat      field.validate(body) -> pydantic
//                        None + required     -> missing
//                        not a mapping       -> dict_type
//                        dict[str, str]      -> string_type per bad VALUE
//
// Every failure comes back `422` from the installed
// `request_validation_exception_handler`. Ten reference handlers carry such a
// parameter (enumerated in the ledger at DIV-367), and before this the port had
// SIX private spellings of the check between them — four of which answered a
// `400` from the handler's own guard for a request the reference never let in.
// A status is what a caller branches on, so that was strictly worse than
// DIV-053's "the 422 body is approximate".
//
// **Every shape below was MEASURED against the reference on `.parity-state/
// fresh` (2026-08-02), never transcribed** — DIV-127's lesson is that an error
// shape no probe issued is a guess, and this file had been carrying one for two
// waves. The probe that produced them is `parity/endpoint-cases.txt`'s `V-*`
// block, which re-measures them on every gate run.
//
// The one that could not have been guessed: a body of the four bytes `null` is
// **`missing`**, not `dict_type`. FastAPI hands pydantic `None`, and `None`
// against a required field is "no value supplied" — the container check never
// runs. An empty body is the same answer by a different road.

/// One pydantic error object, in pydantic's own key order.
fn error_entry(kind: &str, loc: Vec<Value>, msg: &str, input: Value, ctx: Option<Value>) -> Value {
    let mut entry = serde_json::Map::new();
    entry.insert("type".to_owned(), Value::from(kind));
    entry.insert("loc".to_owned(), Value::Array(loc));
    entry.insert("msg".to_owned(), Value::from(msg));
    entry.insert("input".to_owned(), input);
    if let Some(ctx) = ctx {
        entry.insert("ctx".to_owned(), ctx);
    }
    Value::Object(entry)
}

/// `{"detail": [ … ]}` — the body `request_validation_exception_handler`
/// renders.
fn detail_object(entries: Vec<Value>) -> Value {
    let mut obj = serde_json::Map::new();
    obj.insert("detail".to_owned(), Value::Array(entries));
    Value::Object(obj)
}

/// [`detail_object`] at `422` — the whole response.
fn detail_list(entries: Vec<Value>) -> JsonBody {
    JsonBody::with_status(StatusCode::UNPROCESSABLE_ENTITY, detail_object(entries))
}

/// `missing` — no body at all, or the literal `null`, for a REQUIRED parameter.
fn missing_body() -> JsonBody {
    detail_list(vec![error_entry(
        "missing",
        vec![Value::from("body")],
        "Field required",
        Value::Null,
        None,
    )])
}

/// `json_invalid` — the one FastAPI builds by hand, not pydantic.
///
/// `loc` carries CPython's character offset `e.pos` and `ctx.error` carries
/// `e.msg`; [`crate::services::json_error`] is the deduped owner of both (law
/// 9). `input` is a hard-coded empty OBJECT, not the unparseable text.
///
/// **This leg does not care what the parameter is annotated as.**
/// `fastapi/routing.py` calls `await request.json()` for every JSON body it is
/// handed — `dict`, `dict[str, str]` and a pydantic `BaseModel` alike — and
/// catches CPython's `JSONDecodeError` itself, before pydantic is reached at
/// all. So the three model-bodied handlers (`PUT /api/budgets`,
/// `POST /api/patterns/dismiss`, `POST /api/optimize/claudemd-preview`) share
/// this exact shape with the ten dict-bodied ones, and `optimize.rs`'s note
/// that "both the offset and the message come from pydantic-core's own parser"
/// was a reasonable guess that the probe disproved.
#[must_use]
pub fn json_invalid_body(raw: &[u8]) -> JsonBody {
    JsonBody::with_status(StatusCode::UNPROCESSABLE_ENTITY, json_invalid_detail(raw))
}

/// [`json_invalid_body`]'s `{"detail": [ … ]}` object, for the one caller that
/// wraps its own response (`routes/optimize.rs` collects a whole error list).
#[must_use]
pub fn json_invalid_detail(raw: &[u8]) -> Value {
    let text = String::from_utf8_lossy(raw);
    // Unreachable in practice: this runs only because `serde_json` refused the
    // body, and everything it refuses CPython refuses too EXCEPT the three
    // extended literals (`NaN`, `Infinity`, `-Infinity`), which CPython accepts.
    // Those arrive with no CPython error to report — a recorded narrowing, not a
    // panic.
    let (pos, message) = crate::services::json_error::decode_error(&text)
        .unwrap_or((0, "Expecting value".to_owned()));
    let mut ctx = serde_json::Map::new();
    ctx.insert("error".to_owned(), Value::from(message));
    detail_object(vec![error_entry(
        "json_invalid",
        vec![
            Value::from("body"),
            Value::from(i64::try_from(pos).unwrap_or(i64::MAX)),
        ],
        "JSON decode error",
        Value::Object(serde_json::Map::new()),
        Some(Value::Object(ctx)),
    )])
}

/// The shared walk. `optional` is `body: dict | None = None`; `string_values`
/// is `dict[str, str]` rather than `dict` / `dict[str, Any]`.
fn parse_dict_body(
    raw: &[u8],
    string_values: bool,
    optional: bool,
) -> Result<Option<serde_json::Map<String, Value>>, JsonBody> {
    // `if body_bytes:` — an absent body is NO VALUE, and never reaches the
    // decoder. A body of two spaces is not absent: it is a `json_invalid` at
    // offset 2, which is why this is `is_empty` and not a trim.
    if raw.is_empty() {
        return if optional {
            Ok(None)
        } else {
            Err(missing_body())
        };
    }
    let parsed: Value = match serde_json::from_slice(raw) {
        Ok(value) => value,
        Err(_) => return Err(json_invalid_body(raw)),
    };
    match parsed {
        // MEASURED: `null` is `missing`, not `dict_type` — see the note above.
        Value::Null => {
            if optional {
                Ok(None)
            } else {
                Err(missing_body())
            }
        }
        Value::Object(map) => {
            if string_values {
                // pydantic validates the WHOLE mapping and reports every
                // failure, in body order — not just the first. Measured on
                // `{"z": 1, "a": 2}`, which comes back `z` then `a` and so
                // depends on `serde_json`'s `preserve_order` feature being on.
                let entries: Vec<Value> = map
                    .iter()
                    .filter(|(_, value)| !value.is_string())
                    .map(|(key, value)| {
                        error_entry(
                            "string_type",
                            vec![Value::from("body"), Value::from(key.clone())],
                            "Input should be a valid string",
                            value.clone(),
                            None,
                        )
                    })
                    .collect();
                if !entries.is_empty() {
                    return Err(detail_list(entries));
                }
            }
            Ok(Some(map))
        }
        other => Err(detail_list(vec![error_entry(
            "dict_type",
            vec![Value::from("body")],
            "Input should be a valid dictionary",
            other,
            None,
        )])),
    }
}

/// `data: dict` / `data: dict[str, Any]` — a required JSON **object**.
///
/// The values are unconstrained: `{"session_id": 3}` reaches the handler, and
/// the row that proves it is `V-bm-put-int`.
///
/// # Errors
/// The rendered `422`, ready to return as the response.
pub fn dict_body(raw: &[u8]) -> Result<serde_json::Map<String, Value>, JsonBody> {
    parse_dict_body(raw, false, false).map(|parsed| parsed.unwrap_or_default())
}

/// `data: dict[str, str]` — an object whose every VALUE is a string.
///
/// # Errors
/// The rendered `422`, ready to return as the response.
pub fn str_dict_body(raw: &[u8]) -> Result<serde_json::Map<String, Value>, JsonBody> {
    parse_dict_body(raw, true, false).map(|parsed| parsed.unwrap_or_default())
}

/// `body: dict | None = None` — an absent body and a `null` body are both legal
/// and both mean `None`.
///
/// # Errors
/// The rendered `422`, ready to return as the response.
pub fn optional_dict_body(raw: &[u8]) -> Result<Option<serde_json::Map<String, Value>>, JsonBody> {
    parse_dict_body(raw, false, true)
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

    /// Every byte below was measured against the reference on 2026-08-02 and is
    /// re-measured on every gate run by the `V-*` rows — see the DIV-367 block
    /// in `parity/endpoint-cases.txt`. The point of the test is that the class
    /// has ONE implementation now, so this is the only place the shapes live.
    #[test]
    fn the_dict_body_validator_is_pydantics_shapes_and_not_a_handlers_four_hundred() {
        let reject = |raw: &[u8]| str_dict_body(raw).expect_err("rejected").render();

        // `dict[str, str]` — one entry per offending VALUE, in body order.
        assert_eq!(
            reject(br#"{"project_path": 123}"#),
            r#"{"detail":[{"type":"string_type","loc":["body","project_path"],"msg":"Input should be a valid string","input":123}]}"#
        );
        assert_eq!(
            reject(br#"{"project_path": null}"#),
            r#"{"detail":[{"type":"string_type","loc":["body","project_path"],"msg":"Input should be a valid string","input":null}]}"#
        );
        // Insertion order, not sorted order — this is what the workspace's
        // `preserve_order` feature is load-bearing for. Measured: `z` then `a`.
        assert_eq!(
            reject(br#"{"z": 1, "a": 2}"#),
            r#"{"detail":[{"type":"string_type","loc":["body","z"],"msg":"Input should be a valid string","input":1},{"type":"string_type","loc":["body","a"],"msg":"Input should be a valid string","input":2}]}"#
        );
        // A valid value alongside an invalid one still fails, and only the
        // invalid key is reported.
        assert_eq!(
            reject(br#"{"project_path": "", "x": 3}"#),
            r#"{"detail":[{"type":"string_type","loc":["body","x"],"msg":"Input should be a valid string","input":3}]}"#
        );

        // The container half, shared with `dict` and `dict[str, Any]`.
        assert_eq!(
            reject(b"[]"),
            r#"{"detail":[{"type":"dict_type","loc":["body"],"msg":"Input should be a valid dictionary","input":[]}]}"#
        );
        assert_eq!(
            reject(b"3"),
            r#"{"detail":[{"type":"dict_type","loc":["body"],"msg":"Input should be a valid dictionary","input":3}]}"#
        );
        assert_eq!(
            reject(b"true"),
            r#"{"detail":[{"type":"dict_type","loc":["body"],"msg":"Input should be a valid dictionary","input":true}]}"#
        );

        // An object whose values are all strings gets through — including `{}`.
        assert!(str_dict_body(br#"{"project_path": "/tmp"}"#).is_ok());
        assert!(str_dict_body(b"{}").is_ok());
    }

    /// The two shapes that are NOT pydantic's: FastAPI builds both by hand,
    /// before the annotation matters at all.
    #[test]
    fn an_absent_body_and_a_null_body_are_both_missing_and_never_dict_type() {
        let missing =
            r#"{"detail":[{"type":"missing","loc":["body"],"msg":"Field required","input":null}]}"#;
        assert_eq!(dict_body(b"").expect_err("rejected").render(), missing);
        assert_eq!(str_dict_body(b"").expect_err("rejected").render(), missing);
        // The four bytes `null`. pydantic is handed `None`, and `None` against a
        // required field is "no value supplied" — the container check never
        // runs. This is the shape no transcription had got right.
        assert_eq!(dict_body(b"null").expect_err("rejected").render(), missing);
        assert_eq!(
            str_dict_body(b"null").expect_err("rejected").render(),
            missing
        );

        // `body: dict | None = None` — the same two inputs are LEGAL, and there
        // is no `missing` leg on that endpoint at all.
        assert_eq!(optional_dict_body(b"").expect("legal"), None);
        assert_eq!(optional_dict_body(b"null").expect("legal"), None);
        assert_eq!(
            optional_dict_body(b"[]").expect_err("rejected").render(),
            r#"{"detail":[{"type":"dict_type","loc":["body"],"msg":"Input should be a valid dictionary","input":[]}]}"#
        );
    }

    #[test]
    fn json_invalid_carries_cpythons_offset_and_cpythons_wording() {
        let reject = |raw: &[u8]| dict_body(raw).expect_err("rejected").render();
        assert_eq!(
            reject(b"nope"),
            r#"{"detail":[{"type":"json_invalid","loc":["body",0],"msg":"JSON decode error","input":{},"ctx":{"error":"Expecting value"}}]}"#
        );
        // Offset 1 and a different one of the nine messages — the pair the
        // hard-coded `(0, "Expecting value")` in `routes/optimize.rs` got wrong.
        assert_eq!(
            reject(b"{oops"),
            r#"{"detail":[{"type":"json_invalid","loc":["body",1],"msg":"JSON decode error","input":{},"ctx":{"error":"Expecting property name enclosed in double quotes"}}]}"#
        );
        // Whitespace is not an absent body: two spaces fail at offset 2.
        assert_eq!(
            reject(b"  "),
            r#"{"detail":[{"type":"json_invalid","loc":["body",2],"msg":"JSON decode error","input":{},"ctx":{"error":"Expecting value"}}]}"#
        );
    }

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
