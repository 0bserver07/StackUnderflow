//! `routes/playback.py` — 3 endpoints, wave 5 (batch E). **DIV-140 CLOSED.**
//!
//! | Item | Method | FastAPI path | axum path | State |
//! |---|---|---|---|---|
//! | `RS-5-091` | `GET` | `/api/playback/{session_id}/fs       ` | same | ported |
//! | `RS-5-092` | `GET` | `/api/playback/{session_id}          ` | same | ported |
//! | `RS-5-093` | `GET` | `/api/playback/project/{project_slug}` | same | ported |
//!
//! Three thin routes over 1,678 lines of service:
//! [`crate::services::playback`] (882 — the tool-call event extractor over
//! `messages.raw_json`), [`crate::services::playback_fs`] (617 — the v2 virtual
//! filesystem) and [`crate::services::risk`] (179 — the per-file overlay, which
//! turned out to be forty lines of glue over an already-ported `stax-core`
//! function). The route layer is genuinely thin: a comma-split filter, an
//! `Nd|Nh|Nm`-or-ISO `since` parser, four validated query parameters and three
//! `JSONResponse` wrappers.
//!
//! # No wall clock reaches any payload — which is what makes these measurable
//!
//! Checked first, before a line was ported, because DIV-085 permanently opened
//! nineteen `/api/compare` rows for exactly this. `/fs` echoes the *request's*
//! `at`; the session and project bodies carry only store-derived values. The
//! ONE clock read in the module is `_parse_since`'s `datetime.now(UTC)`, and it
//! becomes a SQL lower bound, never a field. A relative `?since=7d` therefore
//! computes bounds a few milliseconds apart on the two servers and can move a
//! row whose timestamp falls in that gap — the case file uses `since=9999d`,
//! whose bound lands in 1999 and cannot.
//!
//! # `/api/playback/project/fs` belongs to the **fs** handler
//!
//! Starlette matches routes in *registration* order, and
//! `/api/playback/{session_id}/fs` is registered first, so
//! `GET /api/playback/project/fs` reaches `get_session_fs_snapshot` with
//! `session_id == "project"` (measured against fastapi 0.141.1 — it answers the
//! `missing`-`at` 422, not a project 404).
//!
//! axum's router is a radix trie that prefers a *static* segment over a
//! parameter, so it would route the same path to `get_project_timeline` with
//! `project_slug == "fs"` and answer a 404. [`get_project_timeline`] therefore
//! carries an explicit shadow check, and `the_fs_route_shadows_a_project_named_fs`
//! pins it. This costs a project legitimately named `fs` its timeline — which
//! is precisely what it costs in Python.
//!
//! # `schema.apply(conn)` is not ported — DIV-106
//!
//! All three Python handlers run a migration on every request, guarding the
//! fresh-install case where a request beats the lifespan hook. The port never
//! migrates a store. Payload-neutral and checked rather than assumed:
//! `a_store_that_never_had_a_schema_answers_the_same_bodies` drives all three
//! endpoints against a store with no tables and gets the reference bodies.
//!
//! # The 422 shapes here are measured, not transcribed (LAW 6)
//!
//! `limit: int = Query(1000, ge=1, le=10_000)` produces error `type`s that no
//! ported route had issued before — `greater_than_equal` / `less_than_equal`,
//! each carrying a `ctx` object the type errors do not. Every body below was
//! taken from the reference interpreter through `TestClient`, including the
//! multi-error ordering (declaration order) and the fact that `input` is the
//! raw **string** for a query parameter where it is the decoded value for a
//! body field.

use axum::Router;
use axum::extract::{Path as PathParam, RawQuery, State};
use axum::http::StatusCode;
use axum::routing::get;
use rusqlite::Connection;
use serde_json::{Map, Value};
use stax_etl::stats::pydatetime::civil_from_epoch;
use stax_etl::stats::pytext::{is_py_space, py_strip};

use crate::json::{HandlerResult, HttpError, JsonBody, join_failure, missing_query_param};
use crate::qs::Query;
use crate::services::playback as playback_service;
use crate::services::playback_fs::{self as fs_service, ReconstructError};
use crate::services::risk as risk_service;
use crate::state::AppState;

/// `_UNIT_SECONDS`.
const UNIT_SECONDS_DAY: i64 = 86_400;
const UNIT_SECONDS_HOUR: i64 = 3_600;
const UNIT_SECONDS_MINUTE: i64 = 60;

/// `limit: int = Query(1000, ge=1, le=10_000)` — the session endpoint.
const SESSION_LIMIT_DEFAULT: i64 = 1000;
const SESSION_LIMIT_MAX: i64 = 10_000;

/// `limit: int = Query(5000, ge=1, le=20_000)` — the project endpoint.
const PROJECT_LIMIT_DEFAULT: i64 = 5000;
const PROJECT_LIMIT_MAX: i64 = 20_000;

/// The static second segment that shadows a project slug — see the module docs.
const FS_SHADOWED_SLUG: &str = "fs";

/// Mount this module's endpoints onto `router`.
///
/// The three path strings are copies of FastAPI's, brace syntax included. Order
/// is irrelevant to axum (its trie has a fixed precedence) and load-bearing to
/// starlette — the shadow shim in [`get_project_timeline`] is what reconciles
/// the two.
pub fn register(router: Router<AppState>) -> Router<AppState> {
    router
        .route(
            "/api/playback/{session_id}/fs",
            get(get_session_fs_snapshot),
        )
        .route("/api/playback/{session_id}", get(get_session_playback))
        .route(
            "/api/playback/project/{project_slug}",
            get(get_project_timeline),
        )
}

// ── FastAPI's validation errors, in the two shapes this module needs ─────────
//
// FLAGGED FOR THE ARCHITECT'S DEDUP LIST: `json::validation_detail` and
// `json::missing_query_param` each build ONE entry with no `ctx`, and there is
// no owner for a multi-entry list or for the `ge`/`le` shapes. `routes/optimize.rs`
// carries a private `error_entry`/`detail_list` pair for the request-BODY
// version of the same thing. These two belong beside them in `json.rs`; they
// are here because batch E may not edit that file.

/// One pydantic error object — `type`, `loc`, `msg`, `input`, then `ctx`.
fn error_entry(kind: &str, field: &str, msg: &str, input: Value, ctx: Option<Value>) -> Value {
    let mut entry = Map::new();
    entry.insert("type".to_owned(), Value::from(kind));
    entry.insert(
        "loc".to_owned(),
        Value::Array(vec![Value::from("query"), Value::from(field)]),
    );
    entry.insert("msg".to_owned(), Value::from(msg));
    entry.insert("input".to_owned(), input);
    if let Some(ctx) = ctx {
        entry.insert("ctx".to_owned(), ctx);
    }
    Value::Object(entry)
}

/// `{"detail": [ … ]}` at `422`. Errors appear in **declaration order**, which
/// is the order FastAPI's dependency solver walks the signature.
fn validation_422(entries: Vec<Value>) -> JsonBody {
    let mut obj = Map::new();
    obj.insert("detail".to_owned(), Value::Array(entries));
    JsonBody::with_status(StatusCode::UNPROCESSABLE_ENTITY, Value::Object(obj))
}

/// pydantic v2 lax `str` → `int`, at arbitrary precision.
///
/// Measured, not assumed: `" 5 "` → 5, `"+5"` → 5, `"1_000"` → 1000 (CPython's
/// `int()` accepts single underscores between digits), `"5.0"` → 5, `"5.5"` →
/// `int_parsing`, `"1e4"` → `int_parsing`, `"0x10"` → `int_parsing`, `""` →
/// `int_parsing`, and `"99999999999999999999"` parses fine and then fails the
/// **bound**, reporting `less_than_equal` rather than a parse error.
///
/// The `i128` is a saturating stand-in for CPython's unbounded `int`: every
/// bound in this module is inside `1..=20_000`, so a value that overflows
/// `i128` compares the same way saturated as it would exactly.
///
/// **Why not `crate::qs::opt_int`.** That helper is `i64`-only and rejects the
/// `"5.0"` form outright — the DIV-107 defect (`!CR-at-float` / `!CR-at-bignum`)
/// the batch-E claim reserves for the architect. It also cannot express a
/// `ge`/`le` failure, which is the whole reason a local coercion exists here.
/// When DIV-107 is fixed in `qs.rs` this function keeps only its bound checks.
fn parse_lax_int(raw: &str) -> Option<i128> {
    let text = py_strip(raw);
    let (negative, body) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text.strip_prefix('+').unwrap_or(text)),
    };
    // The float form: `<digits>.<zeros>`. Checked first only so the fractional
    // part can be discarded before the digit-group scan.
    let digits = match body.split_once('.') {
        Some((whole, frac)) => {
            if !frac.chars().all(|ch| ch == '0') {
                return None;
            }
            whole
        }
        None => body,
    };
    // `int()` allows single underscores BETWEEN digits: not leading, not
    // trailing, never doubled.
    let mut cleaned = String::with_capacity(digits.len());
    let mut previous_underscore = true;
    for ch in digits.chars() {
        if ch == '_' {
            if previous_underscore {
                return None;
            }
            previous_underscore = true;
            continue;
        }
        if !ch.is_ascii_digit() {
            return None;
        }
        previous_underscore = false;
        cleaned.push(ch);
    }
    if cleaned.is_empty() || previous_underscore {
        return None;
    }
    let magnitude = cleaned.parse::<i128>().unwrap_or(i128::MAX);
    Some(if negative { -magnitude } else { magnitude })
}

/// `limit: int = Query(default, ge=…, le=…)` — the value, or the error entry.
fn bounded_int(query: &Query, field: &str, default: i64, max: i64) -> Result<i64, Value> {
    let Some(raw) = query.get(field) else {
        return Ok(default);
    };
    let Some(value) = parse_lax_int(raw) else {
        return Err(error_entry(
            "int_parsing",
            field,
            "Input should be a valid integer, unable to parse string as an integer",
            Value::from(raw),
            None,
        ));
    };
    // The bounds run only when the coercion succeeded, and `input` echoes the
    // ORIGINAL string, not the coerced number.
    if value < 1 {
        let mut ctx = Map::new();
        ctx.insert("ge".to_owned(), Value::from(1));
        return Err(error_entry(
            "greater_than_equal",
            field,
            "Input should be greater than or equal to 1",
            Value::from(raw),
            Some(Value::Object(ctx)),
        ));
    }
    if value > i128::from(max) {
        let mut ctx = Map::new();
        ctx.insert("le".to_owned(), Value::from(max));
        return Err(error_entry(
            "less_than_equal",
            field,
            &format!("Input should be less than or equal to {max}"),
            Value::from(raw),
            Some(Value::Object(ctx)),
        ));
    }
    Ok(i64::try_from(value).unwrap_or(default))
}

/// `flag: bool = Query(default)` — the value, or the error entry.
fn bounded_bool(query: &Query, field: &str, default: bool) -> Result<bool, Value> {
    query.bool_or(field, default).map_err(|err| {
        error_entry(
            "bool_parsing",
            field,
            "Input should be a valid boolean, unable to interpret input",
            Value::from(err.input),
            None,
        )
    })
}

// ── the three query-parameter parsers ────────────────────────────────────────

/// `_parse_tool_filter` — `"Edit,Write"` → `["Edit", "Write"]`; blank → `None`.
///
/// Also `_parse_paths_param`, which is the identical function under a second
/// name in the reference. Kept as one here: two names for one body would be a
/// transcription of a copy-paste, not of a behaviour.
fn parse_comma_list(raw: Option<&str>) -> Option<Vec<String>> {
    // `if not raw:` — truthiness, so an EMPTY parameter is `None`, not `[""]`.
    let raw = raw.filter(|value| !value.is_empty())?;
    let cleaned: Vec<String> = raw
        .split(',')
        .map(|part| py_strip(part).to_owned())
        .filter(|part| !part.is_empty())
        .collect();
    // `cleaned or None`.
    (!cleaned.is_empty()).then_some(cleaned)
}

/// `_parse_since` — a relative `Nd|Nh|Nm` spec, or a literal passed through.
///
/// `^\s*(\d+)\s*([dhm])\s*$` with `re.IGNORECASE`. Anything unrecognised is
/// treated as a literal (`return raw.strip()`); the SQL comparison then simply
/// does not match, which is harmless and is the reference's stated intent.
///
/// **Narrowing (recorded).** `int(m.group(1))` is unbounded in CPython and
/// `timedelta(seconds=…)` raises `OverflowError` past ~999,999,999 days —
/// uncaught, a 500. This saturates instead. `?since=9999d` (the case row) is
/// nine orders of magnitude inside the limit.
fn parse_since(raw: Option<&str>, now_micros: i64) -> Option<String> {
    // `if not raw or not raw.strip(): return None`.
    let raw = raw.filter(|value| !py_strip(value).is_empty())?;
    if let Some(seconds) = relative_seconds(raw) {
        return Some(isoformat_utc(
            now_micros.saturating_sub(seconds.saturating_mul(1_000_000)),
        ));
    }
    Some(py_strip(raw).to_owned())
}

/// `_SINCE_RELATIVE.match(raw)` → the window in seconds.
///
/// `\s` in a `str` pattern is CPython's `Py_UNICODE_ISSPACE` (`is_py_space`),
/// which is wider than `char::is_whitespace` by the four C0 separators — the
/// same class `str.strip()` uses, and the reason both are spelled the same way.
fn relative_seconds(raw: &str) -> Option<i64> {
    // `\s*` at three points; the class and the scan are shared.
    let skip_space = |text: &str, index: usize| {
        let mut index = index;
        for ch in text[index..].chars() {
            if !is_py_space(ch) {
                break;
            }
            index += ch.len_utf8();
        }
        index
    };
    let bytes = raw.as_bytes();
    let mut index = skip_space(raw, 0);
    let digits_start = index;
    while bytes.get(index).is_some_and(u8::is_ascii_digit) {
        index += 1;
    }
    if index == digits_start {
        return None;
    }
    let amount: i64 = raw[digits_start..index].parse().unwrap_or(i64::MAX);
    index = skip_space(raw, index);
    // `[dhm]` with `re.IGNORECASE` — one ASCII letter.
    let unit = bytes.get(index)?.to_ascii_lowercase();
    index += 1;
    index = skip_space(raw, index);
    // `$` also matches just before a single trailing newline, but `\s*` has
    // already eaten it, so "end of string" is the only remaining test.
    if index != bytes.len() {
        return None;
    }
    match unit {
        b'd' => Some(amount.saturating_mul(UNIT_SECONDS_DAY)),
        b'h' => Some(amount.saturating_mul(UNIT_SECONDS_HOUR)),
        b'm' => Some(amount.saturating_mul(UNIT_SECONDS_MINUTE)),
        _ => None,
    }
}

/// `datetime.isoformat()` for an aware UTC value — the `.ffffff` field is
/// omitted when the microsecond is zero, exactly as CPython omits it.
///
/// FLAGGED FOR THE DEDUP LIST: `routes/cost.rs` carries a private
/// `isoformat_utc` with the same body, and `routes/pricing.rs` a third variant.
fn isoformat_utc(micros: i64) -> String {
    let seconds = micros.div_euclid(1_000_000);
    let fraction = micros.rem_euclid(1_000_000);
    let (year, month, day, hour, minute, second) = civil_from_epoch(seconds);
    let stamp = format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}");
    if fraction == 0 {
        format!("{stamp}+00:00")
    } else {
        format!("{stamp}.{fraction:06}+00:00")
    }
}

/// `datetime.now(UTC)` as microseconds since the epoch.
fn now_micros() -> i64 {
    i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_micros(),
    )
    .unwrap_or(0)
}

// ── GET /api/playback/{session_id}/fs ────────────────────────────────────────

/// `get_session_fs_snapshot` — the file contents for `session_id` at `at`.
///
/// * `404` — session not in store.
/// * `422` — `at` absent, `at` unparseable, or `include_content` uncoercible.
/// * `200` — `files` may be empty when the session issued no file-touching
///   calls before `at`.
async fn get_session_fs_snapshot(
    State(state): State<AppState>,
    PathParam(session_id): PathParam<String>,
    RawQuery(raw): RawQuery,
) -> HandlerResult {
    fs_snapshot(
        &state,
        &session_id,
        &Query::parse(raw.as_deref().unwrap_or_default()),
    )
    .await
}

/// The `/fs` handler body, callable from the project route's shadow shim.
async fn fs_snapshot(state: &AppState, session_id: &str, query: &Query) -> HandlerResult {
    // Declaration order: `at`, `paths`, `include_content`. `paths: str | None`
    // never fails to coerce, so only two can contribute an error.
    let mut errors: Vec<Value> = Vec::new();
    let at = query.get("at").map(str::to_owned);
    if at.is_none() {
        // `at: str = Query(...)` — a `missing`, with a null `input`.
        let Value::Object(detail) = missing_query_param("at") else {
            unreachable!("missing_query_param builds an object")
        };
        if let Some(Value::Array(entries)) = detail.get("detail") {
            errors.extend(entries.iter().cloned());
        }
    }
    let paths = parse_comma_list(query.get("paths"));
    let include_content = match bounded_bool(query, "include_content", true) {
        Ok(value) => value,
        Err(entry) => {
            errors.push(entry);
            true
        }
    };
    if !errors.is_empty() {
        return Ok(validation_422(errors));
    }
    let at = at.unwrap_or_default();

    let worker = state.clone();
    let session_id = session_id.to_owned();
    tokio::task::spawn_blocking(move || {
        fs_snapshot_blocking(&worker, &session_id, &at, paths.as_deref(), include_content)
    })
    .await
    .map_err(|err| join_failure(&err))?
}

/// The store half of `/fs`: reconstruct, then decorate each file with its risk.
fn fs_snapshot_blocking(
    state: &AppState,
    session_id: &str,
    at: &str,
    paths: Option<&[String]>,
    include_content: bool,
) -> HandlerResult {
    let conn = state.connect().map_err(store_500)?;
    // `schema.apply(conn)` deliberately absent — DIV-106, module docs.
    let mut snapshot =
        match fs_service::reconstruct_fs_at(&conn, session_id, at, paths, include_content) {
            Ok(payload) => payload,
            Err(err @ ReconstructError::UnknownSession(_)) => {
                return Err(HttpError::not_found(err.detail().to_owned()));
            }
            Err(err @ ReconstructError::Malformed(_)) => {
                return Err(HttpError::new(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    err.detail().to_owned(),
                ));
            }
        };

    // `for path in files:` — the dict's insertion order, which is first-touch
    // order from the replay. The `risk` key is appended AFTER the four (or
    // five) keys already on the entry, so it is always last.
    let paths_in_order: Vec<String> = snapshot
        .get("files")
        .and_then(Value::as_object)
        .map(|files| files.keys().cloned().collect())
        .unwrap_or_default();
    for path in paths_in_order {
        // `except (ValueError, sqlite3.DatabaseError): continue` — a malformed
        // path or a flaky read must not fail the snapshot endpoint.
        let Some(overlay) = risk_service::file_risk_overlay(&conn, &path) else {
            continue;
        };
        if !overlay.is_noteworthy() {
            continue;
        }
        if let Some(entry) = snapshot
            .get_mut("files")
            .and_then(Value::as_object_mut)
            .and_then(|files| files.get_mut(&path))
            .and_then(Value::as_object_mut)
        {
            entry.insert("risk".to_owned(), overlay.to_value());
        }
    }
    Ok(JsonBody::ok(snapshot))
}

// ── GET /api/playback/{session_id} ───────────────────────────────────────────

/// `get_session_playback` — the ordered tool-call stream for one session.
///
/// `404` when the session is not in the store; `200` with an empty `events`
/// list when it exists but issued no tool calls — so the dashboard can tell
/// "wrong session" from "nothing to play back".
async fn get_session_playback(
    State(state): State<AppState>,
    PathParam(session_id): PathParam<String>,
    RawQuery(raw): RawQuery,
) -> HandlerResult {
    let query = Query::parse(raw.as_deref().unwrap_or_default());
    // Declaration order: `tool_filter`, `limit`, `include_payload`.
    let mut errors: Vec<Value> = Vec::new();
    let tool_filter = parse_comma_list(query.get("tool_filter"));
    let limit = match bounded_int(&query, "limit", SESSION_LIMIT_DEFAULT, SESSION_LIMIT_MAX) {
        Ok(value) => value,
        Err(entry) => {
            errors.push(entry);
            SESSION_LIMIT_DEFAULT
        }
    };
    let include_payload = match bounded_bool(&query, "include_payload", true) {
        Ok(value) => value,
        Err(entry) => {
            errors.push(entry);
            true
        }
    };
    if !errors.is_empty() {
        return Ok(validation_422(errors));
    }

    let worker = state.clone();
    tokio::task::spawn_blocking(move || {
        let conn = worker.connect().map_err(store_500)?;
        let page = playback_service::session_playback_page(
            &conn,
            &session_id,
            tool_filter.as_deref(),
            limit,
            include_payload,
        )
        .map_err(sql_500)?;
        // `if page is None:` — the 404 is raised AFTER the connection closes,
        // which is invisible from outside and reproduced by ordering alone.
        let Some((events, truncated)) = page else {
            return Err(HttpError::not_found(format!(
                "Session not found in store: {session_id}"
            )));
        };
        let mut payload = Map::new();
        // `"session_id": session_id` — the REQUESTED id, not the resolved one.
        payload.insert("session_id".to_owned(), Value::from(session_id));
        payload.insert("total".to_owned(), Value::from(events.len()));
        payload.insert(
            "events".to_owned(),
            Value::Array(
                events
                    .iter()
                    .map(playback_service::playback_event_to_dict)
                    .collect(),
            ),
        );
        payload.insert("truncated".to_owned(), Value::Bool(truncated));
        // The literal's order is `session_id, events, total, truncated`; the
        // insert above computed `total` early to avoid a second walk, so the
        // keys are re-seated here.
        Ok(JsonBody::ok(reorder(
            payload,
            &["session_id", "events", "total", "truncated"],
        )))
    })
    .await
    .map_err(|err| join_failure(&err))?
}

// ── GET /api/playback/project/{project_slug} ─────────────────────────────────

/// `get_project_timeline` — the cross-session stream for one project.
///
/// Carries the `/fs` shadow shim: see the module docs for why
/// `/api/playback/project/fs` is not this endpoint's request at all.
async fn get_project_timeline(
    State(state): State<AppState>,
    PathParam(project_slug): PathParam<String>,
    RawQuery(raw): RawQuery,
) -> HandlerResult {
    let query = Query::parse(raw.as_deref().unwrap_or_default());
    if project_slug == FS_SHADOWED_SLUG {
        // starlette matched `/api/playback/{session_id}/fs` first, with
        // `session_id == "project"`.
        return fs_snapshot(&state, "project", &query).await;
    }

    // Declaration order: `since`, `tool_filter`, `limit`, `include_payload`.
    let mut errors: Vec<Value> = Vec::new();
    let since = parse_since(query.get("since"), now_micros());
    let tool_filter = parse_comma_list(query.get("tool_filter"));
    let limit = match bounded_int(&query, "limit", PROJECT_LIMIT_DEFAULT, PROJECT_LIMIT_MAX) {
        Ok(value) => value,
        Err(entry) => {
            errors.push(entry);
            PROJECT_LIMIT_DEFAULT
        }
    };
    // `include_payload: bool = Query(False)` — the default is OFF here, because
    // a project-wide stream is large.
    let include_payload = match bounded_bool(&query, "include_payload", false) {
        Ok(value) => value,
        Err(entry) => {
            errors.push(entry);
            false
        }
    };
    if !errors.is_empty() {
        return Ok(validation_422(errors));
    }

    let worker = state.clone();
    tokio::task::spawn_blocking(move || {
        let conn = worker.connect().map_err(store_500)?;
        let Some(project_id) = get_project_id(&conn, &project_slug).map_err(sql_500)? else {
            return Err(HttpError::not_found(format!(
                "Project not found in store: {project_slug}"
            )));
        };
        let (events, truncated) = playback_service::project_timeline_page(
            &conn,
            project_id,
            since.as_deref(),
            tool_filter.as_deref(),
            limit,
            include_payload,
        )
        .map_err(sql_500)?;
        let mut payload = Map::new();
        payload.insert("project_slug".to_owned(), Value::from(project_slug));
        payload.insert(
            "events".to_owned(),
            Value::Array(
                events
                    .iter()
                    .map(playback_service::playback_event_to_dict)
                    .collect(),
            ),
        );
        payload.insert("total".to_owned(), Value::from(events.len()));
        payload.insert("truncated".to_owned(), Value::Bool(truncated));
        Ok(JsonBody::ok(Value::Object(payload)))
    })
    .await
    .map_err(|err| join_failure(&err))?
}

/// `queries.get_project(conn, slug=…)` — the **first** row, and only its id.
///
/// No `ORDER BY` and no `LIMIT`: `fetchone()` takes whatever SQLite hands over
/// first. The schema's `UNIQUE(provider, slug)` means one slug can name several
/// projects (one per provider), so a multi-provider project's timeline shows
/// **one** provider's sessions — the same narrowing on both sides, and not the
/// `project_ids_for` list-of-ids treatment `/api/cost-data` gives the same slug.
fn get_project_id(conn: &Connection, slug: &str) -> rusqlite::Result<Option<i64>> {
    let mut stmt = conn.prepare(
        "SELECT id, provider, slug, path, display_name, first_seen, last_modified \
         FROM projects WHERE slug = ?",
    )?;
    let mut rows = stmt.query([slug])?;
    match rows.next()? {
        Some(row) => Ok(Some(row.get(0)?)),
        None => Ok(None),
    }
}

/// Re-seat a map's keys into the dict literal's order.
fn reorder(mut source: Map<String, Value>, order: &[&str]) -> Value {
    let mut out = Map::new();
    for key in order {
        if let Some(value) = source.shift_remove(*key) {
            out.insert((*key).to_owned(), value);
        }
    }
    Value::Object(out)
}

fn store_500(err: anyhow::Error) -> HttpError {
    HttpError::new(StatusCode::INTERNAL_SERVER_ERROR, format!("store: {err}"))
}

fn sql_500(err: rusqlite::Error) -> HttpError {
    HttpError::new(StatusCode::INTERNAL_SERVER_ERROR, err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::Config;
    use axum::body::Body;
    use axum::http::Request;
    use serde_json::json;
    use tower::ServiceExt as _;

    /// A scratch `STACKUNDERFLOW_HOME` that cleans itself up.
    struct Scratch(std::path::PathBuf);

    impl Scratch {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "stax-playback-{tag}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |delta| delta.as_nanos())
            ));
            std::fs::create_dir_all(&dir).expect("mkdir");
            Self(dir)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).ok();
        }
    }

    /// One project, one session, one Read/result pair and one Write.
    fn seeded_state(scratch: &Scratch) -> AppState {
        let store = scratch.0.join("store.db");
        let conn = Connection::open(&store).expect("open");
        conn.execute_batch(
            "CREATE TABLE projects (id INTEGER PRIMARY KEY, provider TEXT, slug TEXT,
                 path TEXT, display_name TEXT, first_seen TEXT, last_modified TEXT);
             CREATE TABLE sessions (id INTEGER PRIMARY KEY, project_id INTEGER,
                 session_id TEXT, last_ts TEXT);
             CREATE TABLE messages (id INTEGER PRIMARY KEY, session_fk INTEGER,
                 seq INTEGER, timestamp TEXT, role TEXT, raw_json TEXT);
             INSERT INTO projects (id, provider, slug, path, display_name,
                 first_seen, last_modified)
                 VALUES (1, 'claude', '-p-one', '/p', 'p', '2026-01-01', '2026-01-02');
             INSERT INTO sessions (id, project_id, session_id, last_ts)
                 VALUES (10, 1, 'sess', '2026-01-01T00:00:00Z');",
        )
        .expect("schema");
        let call = json!({"message": {"content": [
            {"type": "tool_use", "id": "r1", "name": "Read",
             "input": {"file_path": "/repo/a.py"}}]}});
        let result = json!({"message": {"content": [
            {"type": "tool_result", "tool_use_id": "r1", "content": "body"}]}});
        conn.execute(
            "INSERT INTO messages (id, session_fk, seq, timestamp, role, raw_json)
             VALUES (1, 10, 1, '2026-01-01T00:00:00Z', 'assistant', ?)",
            [call.to_string()],
        )
        .expect("insert");
        conn.execute(
            "INSERT INTO messages (id, session_fk, seq, timestamp, role, raw_json)
             VALUES (2, 10, 2, '2026-01-01T00:00:01Z', 'user', ?)",
            [result.to_string()],
        )
        .expect("insert");
        drop(conn);
        AppState::new(store, scratch.0.clone(), Config::default())
    }

    /// Drive the mounted routes in-process — no port, so nothing collides with
    /// the reserved `:8095` / `:8096`.
    async fn call(state: &AppState, target: &str) -> (StatusCode, String) {
        let app = register(Router::new()).with_state(state.clone());
        let response = app
            .oneshot(
                Request::builder()
                    .uri(target)
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 22)
            .await
            .expect("body");
        (status, String::from_utf8(bytes.to_vec()).expect("utf-8"))
    }

    // ── the session stream ──────────────────────────────────────────────────

    #[tokio::test]
    async fn the_session_stream_is_the_four_key_body_in_the_literals_order() {
        let scratch = Scratch::new("sess");
        let state = seeded_state(&scratch);
        let (status, body) = call(&state, "/api/playback/sess").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body,
            r#"{"session_id":"sess","events":[{"seq":0,"ts":"2026-01-01T00:00:00Z","message_id":1,"tool_name":"Read","summary":"Read repo/a.py","target_path":"/repo/a.py","byte_count":4,"success":null,"duration_ms":1000,"payload_excerpt":"/repo/a.py\n⇒ body","session_id":"sess"}],"total":1,"truncated":false}"#
        );
    }

    #[tokio::test]
    async fn an_unknown_session_is_the_four_oh_four_with_the_requested_id() {
        let scratch = Scratch::new("sess404");
        let state = seeded_state(&scratch);
        let (status, body) = call(&state, "/api/playback/no-such-session").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(
            body,
            r#"{"detail":"Session not found in store: no-such-session"}"#
        );
    }

    #[tokio::test]
    async fn a_tool_filter_that_matches_nothing_is_an_empty_two_hundred() {
        let scratch = Scratch::new("filter");
        let state = seeded_state(&scratch);
        let (status, body) = call(&state, "/api/playback/sess?tool_filter=Bash").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body,
            r#"{"session_id":"sess","events":[],"total":0,"truncated":false}"#
        );
        // A blank filter is `None`, not an empty set — every event survives.
        let (_, body) = call(&state, "/api/playback/sess?tool_filter=%20,%20").await;
        assert!(body.contains(r#""total":1"#), "{body}");
    }

    #[tokio::test]
    async fn include_payload_off_blanks_the_excerpt() {
        let scratch = Scratch::new("nopayload");
        let state = seeded_state(&scratch);
        let (_, body) = call(&state, "/api/playback/sess?include_payload=0").await;
        assert!(body.contains(r#""payload_excerpt":"""#), "{body}");
    }

    // ── the validation legs, byte for byte ──────────────────────────────────

    #[tokio::test]
    async fn the_limit_bounds_are_pydantics_range_errors_with_their_ctx() {
        let scratch = Scratch::new("bounds");
        let state = seeded_state(&scratch);
        let (status, body) = call(&state, "/api/playback/sess?limit=0").await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(
            body,
            r#"{"detail":[{"type":"greater_than_equal","loc":["query","limit"],"msg":"Input should be greater than or equal to 1","input":"0","ctx":{"ge":1}}]}"#
        );
        let (_, body) = call(&state, "/api/playback/sess?limit=10001").await;
        assert_eq!(
            body,
            r#"{"detail":[{"type":"less_than_equal","loc":["query","limit"],"msg":"Input should be less than or equal to 10000","input":"10001","ctx":{"le":10000}}]}"#
        );
        // The project endpoint's bound is 20_000, and the message carries it.
        let (_, body) = call(&state, "/api/playback/project/-p-one?limit=20001").await;
        assert_eq!(
            body,
            r#"{"detail":[{"type":"less_than_equal","loc":["query","limit"],"msg":"Input should be less than or equal to 20000","input":"20001","ctx":{"le":20000}}]}"#
        );
    }

    #[tokio::test]
    async fn an_uncoercible_limit_is_a_parse_error_and_not_a_range_error() {
        let scratch = Scratch::new("badint");
        let state = seeded_state(&scratch);
        let (status, body) = call(&state, "/api/playback/sess?limit=abc").await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(
            body,
            r#"{"detail":[{"type":"int_parsing","loc":["query","limit"],"msg":"Input should be a valid integer, unable to parse string as an integer","input":"abc"}]}"#
        );
    }

    /// Two bad parameters produce TWO entries, in the signature's order.
    #[tokio::test]
    async fn errors_are_collected_for_every_field_in_declaration_order() {
        let scratch = Scratch::new("multi");
        let state = seeded_state(&scratch);
        let (status, body) = call(&state, "/api/playback/sess?limit=0&include_payload=maybe").await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(
            body,
            r#"{"detail":[{"type":"greater_than_equal","loc":["query","limit"],"msg":"Input should be greater than or equal to 1","input":"0","ctx":{"ge":1}},{"type":"bool_parsing","loc":["query","include_payload"],"msg":"Input should be a valid boolean, unable to interpret input","input":"maybe"}]}"#
        );
    }

    /// pydantic's lax mode, measured through the reference interpreter.
    #[test]
    fn the_lax_int_accepts_what_pydantic_accepts_and_nothing_more() {
        for (raw, want) in [
            ("5", Some(5)),
            ("  5  ", Some(5)),
            ("+5", Some(5)),
            ("-1", Some(-1)),
            ("-0", Some(0)),
            ("1_000", Some(1000)),
            ("5.0", Some(5)),
            ("10000.0", Some(10_000)),
            ("5.5", None),
            ("1e4", None),
            ("0x10", None),
            ("", None),
            (" ", None),
            ("_5", None),
            ("5_", None),
            ("1__0", None),
        ] {
            assert_eq!(parse_lax_int(raw), want.map(i128::from), "{raw:?}");
        }
        // A twenty-digit integer parses and then fails the BOUND, which is why
        // it must not come back as `None`.
        assert_eq!(
            parse_lax_int("99999999999999999999"),
            Some(99_999_999_999_999_999_999_i128)
        );
    }

    // ── the fs snapshot ─────────────────────────────────────────────────────

    #[tokio::test]
    async fn an_absent_at_is_the_missing_four_twenty_two_and_the_handler_never_runs() {
        let scratch = Scratch::new("noat");
        let state = seeded_state(&scratch);
        let (status, body) = call(&state, "/api/playback/sess/fs").await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(
            body,
            r#"{"detail":[{"type":"missing","loc":["query","at"],"msg":"Field required","input":null}]}"#
        );
        // `missing` and a bad bool are BOTH reported, `at` first.
        let (_, body) = call(&state, "/api/playback/sess/fs?include_content=maybe").await;
        assert_eq!(
            body,
            r#"{"detail":[{"type":"missing","loc":["query","at"],"msg":"Field required","input":null},{"type":"bool_parsing","loc":["query","include_content"],"msg":"Input should be a valid boolean, unable to interpret input","input":"maybe"}]}"#
        );
    }

    #[tokio::test]
    async fn an_unparseable_at_is_the_services_four_twenty_two_string_not_a_list() {
        let scratch = Scratch::new("badat");
        let state = seeded_state(&scratch);
        let (status, body) = call(&state, "/api/playback/sess/fs?at=nope").await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        // `HTTPException(422, detail=str(e))` — a one-key object with a STRING
        // detail, unlike the validation list above.
        assert_eq!(
            body,
            r#"{"detail":"Could not parse 'at' as ISO-8601 / RFC-3339: 'nope'"}"#
        );
        // An EMPTY `at` is present-but-unparseable, so it is this 422 and not
        // the `missing` one.
        let (status, body) = call(&state, "/api/playback/sess/fs?at=").await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(
            body,
            r#"{"detail":"Could not parse 'at' as ISO-8601 / RFC-3339: ''"}"#
        );
    }

    #[tokio::test]
    async fn the_fs_snapshot_echoes_the_raw_at_and_carries_no_risk_key() {
        let scratch = Scratch::new("fs");
        let state = seeded_state(&scratch);
        let (status, body) = call(&state, "/api/playback/sess/fs?at=2026-01-01T12:00:00Z").await;
        assert_eq!(status, StatusCode::OK);
        // No `tools_json` / `content_text` columns on this fixture, so the risk
        // lookup fails and is swallowed — the `except … continue` leg.
        assert_eq!(
            body,
            r#"{"session_id":"sess","snapshot_ts":"2026-01-01T12:00:00Z","files":{"/repo/a.py":{"byte_count":4,"last_modified_ts":"2026-01-01T00:00:00Z","operations_applied":["Read#0"],"reconstruction_complete":true,"content":"body"}},"warnings":[]}"#
        );
    }

    #[tokio::test]
    async fn an_fs_snapshot_for_an_unknown_session_is_the_four_oh_four() {
        let scratch = Scratch::new("fs404");
        let state = seeded_state(&scratch);
        let (status, body) = call(&state, "/api/playback/ghost/fs?at=2026-01-01T00:00:00Z").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body, r#"{"detail":"Session not found in store: ghost"}"#);
    }

    // ── the project timeline ────────────────────────────────────────────────

    #[tokio::test]
    async fn the_project_timeline_defaults_include_payload_to_off() {
        let scratch = Scratch::new("proj");
        let state = seeded_state(&scratch);
        let (status, body) = call(&state, "/api/playback/project/-p-one").await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            body.starts_with(r#"{"project_slug":"-p-one","events":["#),
            "{body}"
        );
        assert!(body.contains(r#""payload_excerpt":"""#), "{body}");
        assert!(body.ends_with(r#""total":1,"truncated":false}"#), "{body}");
    }

    #[tokio::test]
    async fn an_unknown_project_slug_is_the_four_oh_four() {
        let scratch = Scratch::new("proj404");
        let state = seeded_state(&scratch);
        let (status, body) = call(&state, "/api/playback/project/-no-such-project").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(
            body,
            r#"{"detail":"Project not found in store: -no-such-project"}"#
        );
    }

    /// The router-precedence reconciliation. Without the shim axum answers a
    /// project 404 here and starlette answers the `missing`-`at` 422.
    #[tokio::test]
    async fn the_fs_route_shadows_a_project_named_fs() {
        let scratch = Scratch::new("shadow");
        let state = seeded_state(&scratch);
        let (status, body) = call(&state, "/api/playback/project/fs").await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(
            body,
            r#"{"detail":[{"type":"missing","loc":["query","at"],"msg":"Field required","input":null}]}"#
        );
        // With an `at`, the shim runs the fs handler for `session_id="project"`
        // — which is not a session, so it is the fs 404.
        let (status, body) = call(&state, "/api/playback/project/fs?at=2026-01-01T00:00:00Z").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body, r#"{"detail":"Session not found in store: project"}"#);
    }

    /// `/api/playback/project` (no slug) is the SESSION route with
    /// `session_id == "project"`, on both routers.
    #[tokio::test]
    async fn a_bare_project_segment_is_a_session_lookup() {
        let scratch = Scratch::new("bare");
        let state = seeded_state(&scratch);
        let (status, body) = call(&state, "/api/playback/project").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body, r#"{"detail":"Session not found in store: project"}"#);
    }

    // ── `since` ─────────────────────────────────────────────────────────────

    #[test]
    fn the_relative_since_grammar_matches_the_regex_and_nothing_else() {
        // 2026-01-01T00:00:00Z as microseconds.
        let now = 1_767_225_600_000_000_i64;
        assert_eq!(
            parse_since(Some("1d"), now).as_deref(),
            Some("2025-12-31T00:00:00+00:00")
        );
        assert_eq!(
            parse_since(Some("  2 H  "), now).as_deref(),
            Some("2025-12-31T22:00:00+00:00")
        );
        assert_eq!(
            parse_since(Some("90m"), now).as_deref(),
            Some("2025-12-31T22:30:00+00:00")
        );
        // Unrecognised → the STRIPPED literal, passed straight through.
        assert_eq!(
            parse_since(Some("  2026-05-01T00:00:00Z "), now).as_deref(),
            Some("2026-05-01T00:00:00Z")
        );
        assert_eq!(parse_since(Some("7 days"), now).as_deref(), Some("7 days"));
        // Blank and absent are both "no lower bound".
        assert_eq!(parse_since(Some(""), now), None);
        assert_eq!(parse_since(Some("   "), now), None);
        assert_eq!(parse_since(None, now), None);
        // The microsecond field appears only when non-zero.
        assert_eq!(
            parse_since(Some("1d"), now + 123_456).as_deref(),
            Some("2025-12-31T00:00:00.123456+00:00")
        );
    }

    #[tokio::test]
    async fn a_since_bound_above_every_message_empties_the_stream() {
        let scratch = Scratch::new("since");
        let state = seeded_state(&scratch);
        let (status, body) = call(&state, "/api/playback/project/-p-one?since=2027-01-01").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body,
            r#"{"project_slug":"-p-one","events":[],"total":0,"truncated":false}"#
        );
        // A relative window nine thousand days wide reaches back past every
        // message, so it is indistinguishable from no bound at all.
        let (_, body) = call(&state, "/api/playback/project/-p-one?since=9999d").await;
        assert!(body.contains(r#""total":1"#), "{body}");
    }

    // ── DIV-106 ─────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn a_store_that_never_had_a_schema_answers_the_same_bodies() {
        // No `schema.apply`, no tables. Python migrates first and then finds
        // nothing; the port finds nothing without migrating. Same three bodies.
        let scratch = Scratch::new("noschema");
        let state = AppState::new(
            scratch.0.join("store.db"),
            scratch.0.clone(),
            Config::default(),
        );
        let (status, body) = call(&state, "/api/playback/anything").await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        // `sessions` does not exist, so the lookup RAISES on both sides — the
        // reference's `schema.apply` would have created it, which is the one
        // observable consequence of DIV-106 and it is a 500 either way.
        assert!(body.contains("no such table"), "{body}");
    }

    #[tokio::test]
    async fn the_router_fallbacks_are_unchanged_by_the_three_new_paths() {
        let scratch = Scratch::new("fallback");
        let state = seeded_state(&scratch);
        // `{session_id}` must not match an empty segment.
        let (status, _) = call(&state, "/api/playback/").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        // Four segments match nothing at all.
        let (status, _) = call(&state, "/api/playback/project/x/fs?at=z").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }
}
