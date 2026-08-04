//! `routes/agent_teams.py` — 3 endpoints, wave 5 (batch E).
//!
//! | Item | Method | FastAPI path | axum path | State |
//! |---|---|---|---|---|
//! | `RS-5-042` | `GET` | `/api/agent-teams` | same | ported |
//! | `RS-5-043` | `GET` | `/api/agent-teams/{session_id}` | same | ported |
//! | `RS-5-044` | `GET` | `/api/agent-teams/{session_id}/agent/{agent_session_id}` | same | ported |
//!
//! This file's previous body was the 25-line deferred stub whose doc comment is
//! where DIV-082 ("a module that answers only its 404 is the shape ruled
//! against") came from. It is closed here.
//!
//! # Scope: the READ path, over whatever the tables already hold
//!
//! **DIV-042 is not this module's to close.** The ingest-side `PostIngestHook`
//! that materialises team metadata is a stub in the port, so `sessions.team_id`
//! and `agent_teams` are written by the *reference's* ingest only. That does not
//! block parity: both servers read ONE shared store, so both see the same 50
//! `agent_teams` rows and the same 321 team-tagged sessions on the harness home,
//! and these three endpoints compare like against like. What DIV-042 costs here
//! is *coverage*, not correctness — a project whose teams were never
//! materialised falls to the heuristic paths on both sides, which is exactly
//! what `A-graph` and `A-list-scoped` exercise.
//!
//! # The one shape that had to be measured
//!
//! `limit: int = Query(50, ge=1, le=500)` is a CONSTRAINED int, and pydantic's
//! bound failure is not `int_parsing`. Measured against the harness reference
//! (fastapi 0.141.1 / pydantic 2.13.4 — `parity/pyserver.py`'s venv), not
//! transcribed:
//!
//! ```text
//! ?limit=0    422 {"detail":[{"type":"greater_than_equal","loc":["query","limit"],
//!                             "msg":"Input should be greater than or equal to 1",
//!                             "input":"0","ctx":{"ge":1}}]}
//! ?limit=501  422 {"detail":[{"type":"less_than_equal", …,"input":"501","ctx":{"le":500}}]}
//! ?limit=abc  422 {"detail":[{"type":"int_parsing", …,"input":"abc"}]}   ← no ctx
//! ```
//!
//! Two things in there are easy to get wrong and are pinned by tests below:
//! `input` echoes the **raw query string**, not the coerced integer (unlike the
//! body-field bounds in `routes/optimize.rs`, where `input` is the JSON value);
//! and `ctx` is present on the bound failures and absent on the parse failure.
//!
//! **Not** DIV-151's `{"detail":"<field>"}`: that pinned-wrong shape lives in
//! `commands` ×2, `cost` and `budgets`, and it is not what this endpoint ships.
//! The parse leg here is `json::validation_422`, the measured list.

use axum::Router;
use axum::extract::{Path as PathParam, RawQuery, State};
use axum::http::StatusCode;
use axum::routing::get;
use serde_json::{Map, Value};

use crate::json::{HandlerResult, HttpError, JsonBody, bound_422, join_failure, validation_422};
use crate::qs::{Query, QueryError};
use crate::services::agent_teams as service;
use crate::state::AppState;

/// `Query(50, ge=1, le=500)` — the default and both bounds.
const LIMIT_DEFAULT: i64 = 50;
const LIMIT_MIN: i64 = 1;
const LIMIT_MAX: i64 = 500;

/// Mount this module's endpoints onto `router`.
///
/// Called once, from [`super::register_all`], at this module's `include_router`
/// position in `server.py`.
pub fn register(router: Router<AppState>) -> Router<AppState> {
    router
        .route("/api/agent-teams", get(list_agent_teams))
        .route("/api/agent-teams/{session_id}", get(get_agent_team))
        .route(
            "/api/agent-teams/{session_id}/agent/{agent_session_id}",
            get(get_agent_team_transcript),
        )
}

// ── shared ───────────────────────────────────────────────────────────────────

fn any_500(err: &anyhow::Error) -> HttpError {
    HttpError::new(StatusCode::INTERNAL_SERVER_ERROR, err.to_string())
}

fn sql_500(err: &rusqlite::Error) -> HttpError {
    HttpError::new(StatusCode::INTERNAL_SERVER_ERROR, err.to_string())
}

/// `limit: int = Query(50, ge=1, le=500)` — coercion then bounds.
///
/// # Why this does not call `qs::Query::opt_int`
///
/// `opt_int` parses into an `i64` and reports `int_parsing` on overflow, but
/// pydantic's integers are arbitrary-precision: `?limit=999999999999999999999`
/// coerces *fine* there and then fails the `le=500` bound. That is the same
/// defect **DIV-107** records against the shared helper (`!CR-at-bignum`), and
/// the shared helper is the architect's to fix — so this module does the
/// coercion locally rather than shipping the wrong 422 for a case row it owns.
/// The lax rules reproduced are pydantic's: surrounding whitespace is stripped,
/// a leading sign is allowed, a fractional string is NOT an integer.
///
/// Returns `Err(the 422 body)`, which the handler returns as `Ok` — a
/// `RequestValidationError` is not an `HTTPException` and its `detail` is a
/// list, not a string.
fn parse_limit(query: &Query) -> Result<i64, JsonBody> {
    let Some(raw) = query.get("limit") else {
        return Ok(LIMIT_DEFAULT);
    };
    let trimmed = raw.trim();
    let value = match trimmed.parse::<i64>() {
        Ok(value) => value,
        Err(_) if is_integer_literal(trimmed) => {
            // Too big for an `i64` but a perfectly good pydantic int. Only the
            // bound comparison follows and both bounds fit in an `i64`, so
            // saturating gives the bound check the same verdict.
            if trimmed.starts_with('-') {
                i64::MIN
            } else {
                i64::MAX
            }
        }
        Err(_) => {
            return Err(validation_422(&QueryError {
                field: "limit".to_owned(),
                input: raw.to_owned(),
                kind: "int_parsing",
            }));
        }
    };
    if value < LIMIT_MIN {
        return Err(bound_422(
            "limit",
            "greater_than_equal",
            "Input should be greater than or equal to 1",
            raw,
            "ge",
            LIMIT_MIN,
        ));
    }
    if value > LIMIT_MAX {
        return Err(bound_422(
            "limit",
            "less_than_equal",
            "Input should be less than or equal to 500",
            raw,
            "le",
            LIMIT_MAX,
        ));
    }
    Ok(value)
}

/// An optional sign followed by at least one ASCII digit — pydantic's lax
/// string→int grammar, minus the `i64` range it does not have.
fn is_integer_literal(text: &str) -> bool {
    let digits = text.strip_prefix(['+', '-']).unwrap_or(text);
    !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit())
}

// ── GET /api/agent-teams ─────────────────────────────────────────────────────

/// `list_agent_teams` — recent sessions that spawned at least one sub-agent.
///
/// An empty store answers `{"teams": []}`; it never 500s on a fresh install.
async fn list_agent_teams(State(state): State<AppState>, RawQuery(raw): RawQuery) -> HandlerResult {
    let query = Query::parse(raw.as_deref().unwrap_or_default());
    let limit = match parse_limit(&query) {
        Ok(limit) => limit,
        Err(body) => return Ok(body),
    };
    // `project: str | None = Query(None)` — ABSENT is `None`, and `?project=`
    // is `Some("")`. The service reads those two differently; see its
    // `indexed_teams_match_project`.
    let project = query.get("project").map(str::to_owned);

    let worker = state.clone();
    let teams = tokio::task::spawn_blocking(move || -> Result<Vec<Value>, HttpError> {
        let conn = worker.connect().map_err(|err| any_500(&err))?;
        // `schema.apply(conn)` runs here in Python — a migration on every
        // request. Not ported (DIV-102); the port never writes DDL.
        let rows = service::list_team_sessions(&conn, limit, project.as_deref())
            .map_err(|err| sql_500(&err))?;
        Ok(rows.iter().map(service::TeamSummary::to_dict).collect())
    })
    .await
    .map_err(|err| join_failure(&err))??;

    let mut payload = Map::new();
    payload.insert("teams".to_owned(), Value::Array(teams));
    Ok(JsonBody::ok(Value::Object(payload)))
}

// ── GET /api/agent-teams/{session_id} ────────────────────────────────────────

/// `get_agent_team` — the full lead → agents graph.
///
/// 404 when no session with that id exists; 200 with an EMPTY `agents` array
/// when it exists but spawned nothing, so the dashboard can tell "wrong url"
/// from "no agents yet".
async fn get_agent_team(
    State(state): State<AppState>,
    PathParam(session_id): PathParam<String>,
) -> HandlerResult {
    let worker = state.clone();
    let wanted = session_id.clone();
    let graph = tokio::task::spawn_blocking(move || -> Result<Option<Value>, HttpError> {
        let conn = worker.connect().map_err(|err| any_500(&err))?;
        // LAW 2 — the store's price book, not `default_engine`.
        let engine =
            crate::pricing::engine(&conn, worker.package_dir()).map_err(|err| any_500(&err))?;
        let graph =
            service::build_team_graph(&conn, &engine, &wanted).map_err(|err| sql_500(&err))?;
        Ok(graph.map(|graph| graph.to_dict()))
    })
    .await
    .map_err(|err| join_failure(&err))??;

    let Some(graph) = graph else {
        return Err(HttpError::not_found(format!(
            "Lead session not found in store: {session_id}"
        )));
    };
    Ok(JsonBody::ok(graph))
}

// ── GET /api/agent-teams/{session_id}/agent/{agent_session_id} ───────────────

/// `get_agent_team_transcript` — one agent's full message list.
///
/// The 404 fires when either session is missing OR when the two live in
/// different projects — one detail string for three different causes, which is
/// what makes `A-transcript` (a missing agent under a real lead) and a
/// cross-project pair indistinguishable on the wire. Reproduced.
async fn get_agent_team_transcript(
    State(state): State<AppState>,
    PathParam((session_id, agent_session_id)): PathParam<(String, String)>,
) -> HandlerResult {
    let worker = state.clone();
    let lead = session_id.clone();
    let agent = agent_session_id.clone();
    let rows = tokio::task::spawn_blocking(move || -> Result<Option<Vec<Value>>, HttpError> {
        let conn = worker.connect().map_err(|err| any_500(&err))?;
        service::get_agent_transcript(&conn, &lead, &agent).map_err(|err| sql_500(&err))
    })
    .await
    .map_err(|err| join_failure(&err))??;

    let Some(rows) = rows else {
        return Err(HttpError::not_found(format!(
            "Agent session {agent_session_id} not found in the same project as lead {session_id}"
        )));
    };
    let count = i64::try_from(rows.len()).unwrap_or(i64::MAX);
    let mut payload = Map::new();
    payload.insert("session_id".to_owned(), Value::from(session_id));
    payload.insert("agent_session_id".to_owned(), Value::from(agent_session_id));
    payload.insert("messages".to_owned(), Value::Array(rows));
    payload.insert("message_count".to_owned(), Value::from(count));
    Ok(JsonBody::ok(Value::Object(payload)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limit_body(raw: &str) -> String {
        match parse_limit(&Query::parse(raw)) {
            Ok(value) => format!("OK {value}"),
            Err(body) => body.render(),
        }
    }

    #[test]
    fn the_default_and_the_inclusive_bounds_are_accepted() {
        assert_eq!(limit_body(""), "OK 50");
        assert_eq!(limit_body("limit=1"), "OK 1");
        assert_eq!(limit_body("limit=500"), "OK 500");
        // pydantic strips surrounding whitespace before coercing.
        assert_eq!(limit_body("limit=%20%205%20"), "OK 5");
        assert_eq!(limit_body("limit=+7"), "OK 7");
        // starlette's `QueryParams._dict` keeps the LAST occurrence.
        assert_eq!(limit_body("limit=3&limit=7"), "OK 7");
    }

    /// The exact bytes the reference answers. Measured with `TestClient`
    /// against fastapi 0.141.1 / pydantic 2.13.4 — the venv
    /// `endpoint-parity.sh` boots — not transcribed from memory (law 6).
    #[test]
    fn the_bound_failures_carry_a_ctx_and_echo_the_raw_string() {
        assert_eq!(
            limit_body("limit=0"),
            r#"{"detail":[{"type":"greater_than_equal","loc":["query","limit"],"msg":"Input should be greater than or equal to 1","input":"0","ctx":{"ge":1}}]}"#
        );
        assert_eq!(
            limit_body("limit=-1"),
            r#"{"detail":[{"type":"greater_than_equal","loc":["query","limit"],"msg":"Input should be greater than or equal to 1","input":"-1","ctx":{"ge":1}}]}"#
        );
        assert_eq!(
            limit_body("limit=501"),
            r#"{"detail":[{"type":"less_than_equal","loc":["query","limit"],"msg":"Input should be less than or equal to 500","input":"501","ctx":{"le":500}}]}"#
        );
    }

    /// An integer far outside `i64` is a BOUND failure, not a parse failure —
    /// pydantic's ints are arbitrary-precision. `qs::opt_int` would answer
    /// `int_parsing` here (DIV-107), which is why this module coerces locally.
    #[test]
    fn a_bignum_limit_fails_the_upper_bound_not_the_parser() {
        assert_eq!(
            limit_body("limit=999999999999999999999"),
            r#"{"detail":[{"type":"less_than_equal","loc":["query","limit"],"msg":"Input should be less than or equal to 500","input":"999999999999999999999","ctx":{"le":500}}]}"#
        );
        // …and the negative bignum fails the LOWER bound.
        assert_eq!(
            limit_body("limit=-999999999999999999999"),
            r#"{"detail":[{"type":"greater_than_equal","loc":["query","limit"],"msg":"Input should be greater than or equal to 1","input":"-999999999999999999999","ctx":{"ge":1}}]}"#
        );
    }

    /// The parse leg carries NO `ctx`, and it is `json::validation_422`'s body —
    /// not DIV-151's `{"detail":"limit"}`.
    #[test]
    fn an_uncoercible_limit_is_the_measured_pydantic_list_and_not_div_151() {
        for raw in ["limit=abc", "limit=5.5", "limit="] {
            let body = limit_body(raw);
            assert!(body.contains(r#""type":"int_parsing""#), "{raw} -> {body}");
            assert!(!body.contains("ctx"), "{raw} -> {body}");
            assert!(!body.starts_with(r#"{"detail":"limit""#), "{raw} -> {body}");
        }
        assert_eq!(
            limit_body("limit=5.5"),
            r#"{"detail":[{"type":"int_parsing","loc":["query","limit"],"msg":"Input should be a valid integer, unable to parse string as an integer","input":"5.5"}]}"#
        );
        assert_eq!(
            limit_body("limit="),
            r#"{"detail":[{"type":"int_parsing","loc":["query","limit"],"msg":"Input should be a valid integer, unable to parse string as an integer","input":""}]}"#
        );
    }

    #[test]
    fn the_integer_literal_grammar_is_sign_then_digits() {
        assert!(is_integer_literal("5"));
        assert!(is_integer_literal("-5"));
        assert!(is_integer_literal("+5"));
        assert!(!is_integer_literal(""));
        assert!(!is_integer_literal("-"));
        assert!(!is_integer_literal("5.5"));
        assert!(!is_integer_literal("5e3"));
        assert!(!is_integer_literal("abc"));
    }

    /// Both 404 detail strings, spelled out. The transcript one is an implicit
    /// concatenation of two f-strings in Python — `"…the same "` + `"project
    /// as lead …"` — and the single space between them is easy to lose.
    #[test]
    fn the_two_404_details_are_verbatim() {
        assert_eq!(
            HttpError::not_found(format!(
                "Lead session not found in store: {}",
                "no-such-session"
            ))
            .body()
            .render(),
            r#"{"detail":"Lead session not found in store: no-such-session"}"#
        );
        assert_eq!(
            HttpError::not_found(format!(
                "Agent session {} not found in the same project as lead {}",
                "no-such-agent", "44b8f238"
            ))
            .body()
            .render(),
            r#"{"detail":"Agent session no-such-agent not found in the same project as lead 44b8f238"}"#
        );
    }
}
