//! `routes/export.py` — 1 endpoint, wave 5.
//!
//! | Item | Method | FastAPI path | axum path | State |
//! |---|---|---|---|---|
//! | `RS-5-073` | `GET` | `/api/export` | `/api/export` | **ported** |
//!
//! Thin, exactly as Python's is: validate two query parameters against their
//! allow-lists, call [`crate::services::export::run_export`], wrap the returned
//! text in a download response. Everything else — the SQL, the pricing, the CSV
//! and JSON writers — is in the service, because `stackunderflow export` calls
//! the same function and wave 8 must find it there rather than fork it.
//!
//! # The one response in the wave that is not `JSONResponse`
//!
//! Every other ported handler returns `crate::json::JsonBody`, which renders
//! through `pyjson::dumps_http` (LAW 1). This one cannot: `run_export` hands
//! back a *string* plus a media type, and starlette's plain `Response` puts that
//! string on the wire verbatim. Two consequences, both measured against the
//! reference rather than reasoned about:
//!
//! * **The `format=json` body is the CLI writer.** `render_export_json` is
//!   `json.dumps(payload, indent=2)` — `ensure_ascii=True`, two-space indent —
//!   so a non-ASCII project name ships as `é`, not as UTF-8. LAW 1 governs
//!   JSON *bodies produced by the response layer*; here the body is produced by
//!   the service and the response layer only carries it.
//! * **`content-type` gets a charset for CSV and not for JSON.**
//!   `Response.init_headers` appends `; charset=utf-8` only when the media type
//!   `startswith("text/")`, so `text/csv` becomes `text/csv; charset=utf-8` and
//!   `application/json` stays bare. The endpoint differ compares this header, so
//!   "helpfully" adding a charset to the JSON leg would be a divergence.
//!
//! Header order is starlette's too — the caller's `headers=` dict first, then
//! `content-length`, then `content-type` (`init_headers` appends in that order),
//! and every name is lower-cased on the way out. `content-length` is the byte
//! length of the UTF-8 body, not the character count.
//!
//! # The two 400s
//!
//! Both `detail` strings interpolate a Python `sorted()` over a **set** through
//! `repr`, which is why they carry single quotes and a space after each comma:
//!
//! ```text
//! format must be one of ['csv', 'json']
//! period must be one of ['all', 'month', 'today', 'week'] or omitted
//! ```
//!
//! A third `ValueError` path exists inside `run_export` (`Unknown period …`) and
//! is unreachable from HTTP, because the route's allow-list is exactly
//! `EXPORT_PERIOD_MAP`'s key set. It is wired anyway, so the day someone widens
//! one list and not the other the answer is a 400 and not a 500.
//!
//! # What this endpoint does NOT do
//!
//! Python opens its own connection and calls `schema.apply(conn)` — a
//! **migration** — on every request. The port does not: it is read-only by
//! campaign law, and applying migrations from a GET handler is the one thing a
//! differ can never make safe. On an up-to-date store `apply` is a no-op, which
//! is the state both servers run against (DIV-132).

use axum::Router;
use axum::body::Body;
use axum::extract::{RawQuery, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde_json::{Map, Value};

use crate::json::{HttpError, JsonBody};
use crate::qs::Query;
use crate::services::export::{Export, ExportError, run_export};
use crate::services::scope::Instant;
use crate::state::AppState;

/// `_VALID_FORMATS` — a **set**, and the 400 message renders it `sorted()`.
const VALID_FORMATS: [&str; 2] = ["csv", "json"];

/// `_VALID_PERIODS` — likewise. Note the vocabulary is the CLI's
/// (`week` / `month`), not `reports/scope.py`'s (`7days` / `30days`).
const VALID_PERIODS: [&str; 4] = ["today", "week", "month", "all"];

/// Mount this module's endpoints onto `router`.
///
/// Called once, from [`super::register_all`], at this module's `include_router`
/// position (eleventh of the 34).
pub fn register(router: Router<AppState>) -> Router<AppState> {
    router.route("/api/export", get(export_endpoint))
}

/// `GET /api/export`.
///
/// Declared `async def` in Python but every line of it blocks (sqlite, and for
/// `format=json` the whole aggregator pipeline once per in-scope project), so it
/// runs on `spawn_blocking` here — the same treatment
/// `routes/data.rs::get_stats` gets, and for the same reason.
async fn export_endpoint(
    State(state): State<AppState>,
    RawQuery(raw): RawQuery,
) -> Result<Response, HttpError> {
    let query = Query::parse(raw.as_deref().unwrap_or_default());

    // `format: str = Query(...)` — REQUIRED, so an absent one is FastAPI's 422
    // before the handler body runs. A repeated `?format=` takes the LAST value
    // (starlette's `QueryParams._dict` is a comprehension), which `Query::get`
    // already does.
    let Some(format) = query.get("format") else {
        return Ok(JsonBody::with_status(
            StatusCode::UNPROCESSABLE_ENTITY,
            missing_query_param("format"),
        )
        .into_response());
    };
    let format = format.to_owned();
    let period = query.get("period").map(str::to_owned);
    let provider = query.get("provider").map(str::to_owned);
    // `list[str] | None` — absent is `None`, not `[]`, and the handler passes
    // `list(project) if project else None` so a present-but-empty list would
    // become `None` too. `opt_list` never returns `Some(vec![])`.
    let include = query.opt_list("project");
    let exclude = query.opt_list("exclude");

    if !VALID_FORMATS.contains(&format.as_str()) {
        return Err(HttpError::bad_request(format!(
            "format must be one of {}",
            sorted_repr(&VALID_FORMATS)
        )));
    }
    // `if period is not None and period not in _VALID_PERIODS` — `?period=` (the
    // empty string) is NOT None, so it reaches this check and 400s.
    if let Some(period) = &period
        && !VALID_PERIODS.contains(&period.as_str())
    {
        return Err(HttpError::bad_request(format!(
            "period must be one of {} or omitted",
            sorted_repr(&VALID_PERIODS)
        )));
    }

    let worker = state.clone();
    let export = tokio::task::spawn_blocking(move || {
        build_export(
            &worker,
            &format,
            period.as_deref(),
            provider.as_deref(),
            include.as_deref(),
            exclude.as_deref(),
        )
    })
    .await
    .map_err(|err| crate::json::join_failure(&err))??;

    Ok(download_response(&export))
}

/// The blocking body: open the store, build the engine, render.
fn build_export(
    state: &AppState,
    format: &str,
    period: Option<&str>,
    provider: Option<&str>,
    include: Option<&[String]>,
    exclude: Option<&[String]>,
) -> Result<Export, HttpError> {
    let conn = state
        .connect()
        .map_err(|err| HttpError::new(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?;
    // LAW 2 / DIV-056: the running server prices from the primed `price_book`,
    // never from the manifest-only `default_engine()`. A 2% gap hides here and
    // no test on an unprimed store can see it.
    let engine = crate::pricing::engine(&conn, state.package_dir())
        .map_err(|err| HttpError::new(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?;

    run_export(
        &conn,
        &engine,
        format,
        period,
        provider,
        include,
        exclude,
        &Instant::now_utc,
    )
    .map_err(|err| match err {
        // `except ValueError as e: raise HTTPException(status_code=400, detail=str(e))`.
        ExportError::Value(msg) => HttpError::bad_request(msg),
        ExportError::Internal(msg) => HttpError::new(StatusCode::INTERNAL_SERVER_ERROR, msg),
    })
}

/// starlette's `Response(content=…, media_type=…, headers={…})`.
///
/// The header order is `init_headers`'s: the caller's dict, then
/// `content-length`, then `content-type`. `content-length` counts BYTES — a CSV
/// carrying an em-dash is longer in bytes than in characters.
fn download_response(export: &Export) -> Response {
    // `Response.init_headers`: the charset is appended only for `text/*`. So
    // `text/csv; charset=utf-8`, and `application/json` bare.
    let content_type = if export.content_type.starts_with("text/") {
        format!("{}; charset=utf-8", export.content_type)
    } else {
        export.content_type.to_owned()
    };
    let disposition = format!("attachment; filename=\"{}\"", export.filename);
    let body = export.text.clone().into_bytes();
    let length = body.len();

    let mut response = Response::new(Body::from(body));
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_DISPOSITION,
        // A filename is `stackunderflow-export-<period>-<date>.<fmt>` — ASCII by
        // construction — so this cannot fail; an unexpected byte drops the
        // header rather than 500-ing a report.
        HeaderValue::from_str(&disposition).unwrap_or_else(|_| HeaderValue::from_static("")),
    );
    headers.insert(
        // The React blob path reads this instead of parsing Content-Disposition.
        "x-suggested-filename",
        HeaderValue::from_str(&export.filename).unwrap_or_else(|_| HeaderValue::from_static("")),
    );
    headers.insert(
        header::CONTENT_LENGTH,
        HeaderValue::from_str(&length.to_string())
            .unwrap_or_else(|_| HeaderValue::from_static("0")),
    );
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(&content_type)
            .unwrap_or_else(|_| HeaderValue::from_static(crate::json::JSON_CONTENT_TYPE)),
    );
    response
}

/// `f"{sorted(some_set)}"` — a Python list of `str` rendered through `repr`.
///
/// Single quotes, `, ` between items, square brackets. Writing `["csv","json"]`
/// (JSON's spelling) would be a byte divergence on both 400 legs.
fn sorted_repr(values: &[&str]) -> String {
    let mut sorted: Vec<&str> = values.to_vec();
    sorted.sort_unstable();
    let items: Vec<String> = sorted.iter().map(|v| format!("'{v}'")).collect();
    format!("[{}]", items.join(", "))
}

/// FastAPI's 422 for an absent required query parameter.
///
/// FLAGGED FOR DEDUP: `routes/sessions.rs::missing_query_param` is identical.
/// Verified byte-for-byte against the reference on `GET /api/export` with no
/// `format` (DIV-053's caveat does not bite for the `missing` shape).
fn missing_query_param(field: &str) -> Value {
    let mut entry = Map::new();
    entry.insert("type".to_owned(), Value::from("missing"));
    entry.insert(
        "loc".to_owned(),
        Value::Array(vec![Value::from("query"), Value::from(field)]),
    );
    entry.insert("msg".to_owned(), Value::from("Field required"));
    entry.insert("input".to_owned(), Value::Null);
    let mut obj = Map::new();
    obj.insert(
        "detail".to_owned(),
        Value::Array(vec![Value::Object(entry)]),
    );
    Value::Object(obj)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_400_details_render_a_python_sorted_set_not_a_json_array() {
        // `f"format must be one of {sorted(_VALID_FORMATS)}"`. Single quotes and
        // a space after the comma — measured on the reference interpreter.
        assert_eq!(sorted_repr(&VALID_FORMATS), "['csv', 'json']");
        // The period set is NOT in declaration order once sorted: `all` first.
        assert_eq!(
            sorted_repr(&VALID_PERIODS),
            "['all', 'month', 'today', 'week']"
        );
        assert_eq!(
            HttpError::bad_request(format!(
                "period must be one of {} or omitted",
                sorted_repr(&VALID_PERIODS)
            ))
            .body()
            .render(),
            r#"{"detail":"period must be one of ['all', 'month', 'today', 'week'] or omitted"}"#
        );
    }

    #[test]
    fn the_missing_format_422_is_fastapis_detail_list() {
        assert_eq!(
            JsonBody::with_status(
                StatusCode::UNPROCESSABLE_ENTITY,
                missing_query_param("format")
            )
            .render(),
            r#"{"detail":[{"type":"missing","loc":["query","format"],"msg":"Field required","input":null}]}"#
        );
    }

    #[test]
    fn the_csv_leg_carries_a_charset_and_the_json_leg_does_not() {
        // `Response.init_headers` appends `; charset=utf-8` only for `text/*`.
        // Getting this "helpfully" right on the JSON leg is a divergence.
        let csv = download_response(&Export {
            text: "a,b\n".to_owned(),
            content_type: "text/csv",
            filename: "stackunderflow-export-all-2026-07-31.csv".to_owned(),
        });
        assert_eq!(
            csv.headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some("text/csv; charset=utf-8")
        );
        let json = download_response(&Export {
            text: "{}".to_owned(),
            content_type: "application/json",
            filename: "stackunderflow-export-all-2026-07-31.json".to_owned(),
        });
        assert_eq!(
            json.headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some("application/json")
        );
    }

    #[test]
    fn the_download_headers_are_the_two_python_sets_plus_a_byte_length() {
        let response = download_response(&Export {
            // An em-dash: three UTF-8 bytes, one character. `content-length` must
            // be 6, not 4 — starlette measures the ENCODED body.
            text: "a—b\n".to_owned(),
            content_type: "text/csv",
            filename: "stackunderflow-export-rollup-2026-07-31.csv".to_owned(),
        });
        assert_eq!(
            response
                .headers()
                .get(header::CONTENT_DISPOSITION)
                .and_then(|v| v.to_str().ok()),
            Some("attachment; filename=\"stackunderflow-export-rollup-2026-07-31.csv\"")
        );
        assert_eq!(
            response
                .headers()
                .get("x-suggested-filename")
                .and_then(|v| v.to_str().ok()),
            Some("stackunderflow-export-rollup-2026-07-31.csv")
        );
        assert_eq!(
            response
                .headers()
                .get(header::CONTENT_LENGTH)
                .and_then(|v| v.to_str().ok()),
            Some("6")
        );
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[test]
    fn a_repeated_format_takes_the_last_occurrence() {
        // starlette's scalar rule, and the reason `?format=csv&format=json`
        // exports JSON. Confirmed against the reference app.
        let query = Query::parse("format=csv&format=json");
        assert_eq!(query.get("format"), Some("json"));
        // …while the repeatable ones accumulate.
        assert_eq!(
            Query::parse("project=a&project=b").opt_list("project"),
            Some(vec!["a".to_owned(), "b".to_owned()])
        );
        // Absent is `None`, which the handler forwards as `include=None`.
        assert!(Query::parse("format=csv").opt_list("project").is_none());
    }

    #[test]
    fn an_empty_period_is_not_none_and_therefore_400s() {
        // `if period is not None and period not in _VALID_PERIODS` — `?period=`
        // arrives as `""`, which is a real value and fails the allow-list. A
        // truthiness check here would have silently exported the rollup instead.
        let query = Query::parse("format=csv&period=");
        assert_eq!(query.get("period"), Some(""));
        assert!(!VALID_PERIODS.contains(&""));
    }

    #[test]
    fn an_empty_format_400s_rather_than_422ing() {
        // `?format=` satisfies the required-parameter check (it is present) and
        // then fails the allow-list — a 400, not a 422. The two error shapes are
        // different objects, so the distinction is visible in the bytes.
        let query = Query::parse("format=");
        assert_eq!(query.get("format"), Some(""));
        assert!(!VALID_FORMATS.contains(&""));
    }
}
