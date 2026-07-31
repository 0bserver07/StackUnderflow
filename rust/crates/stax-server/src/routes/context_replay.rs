//! `routes/context_replay.py` — 1 endpoint, wave 5.
//!
//! | Item | Method | FastAPI path | axum path | State |
//! |---|---|---|---|---|
//! | `RS-5-061` | `GET` | `/api/context-replay/{session_id}` | same | **ported** |
//!
//! A thin wrapper over [`crate::services::context_replay`]: resolve the session,
//! apply the project fence, hand the timeline to the service to build and slice.
//! The response lets the dashboard scrub `?at=<seq>` and watch the context grow
//! turn by turn.
//!
//! # It is advisory: it never 500s and it never 404s
//!
//! This is the property that shapes the whole module. An unknown session id, a
//! store missing its tables, a slug that names no project — every one of them
//! answers `200` with the empty-but-valid body and (where useful) a `warnings`
//! note. Python spells that as an `except Exception` on each store read; the
//! port spells it as a swallow at the same places, and **only** those places. A
//! `?`-propagated `rusqlite::Error` where Python caught one is a behaviour
//! change that shows up on nobody's machine until it shows up on a broken
//! store, so each `.ok()` below names the `except` it mirrors.
//!
//! Two branches are easy to conflate and are not the same code path:
//!
//! * **Unknown session.** The cache is skipped entirely and
//!   `build_context_timeline` is called on an id that resolves to nothing; the
//!   service returns the empty shape with `session not found in store: …`, and
//!   the route *slices* that (so `at_seq` is stamped with the requested value).
//! * **Out-of-scope session.** `empty_context` is called directly with the
//!   fence warning, and it is **not** sliced — same six keys, different
//!   producer.
//!
//! # The cross-project fence is a three-way distinction, not a list
//!
//! `_scope_project_ids` returns `None`, `[]`, or a populated list, and all
//! three mean different things:
//!
//! | Return | Meaning |
//! |---|---|
//! | `None` | no scope is active — the whole store, no fence |
//! | `[]` | a scope was requested and matched no project — fence **everything** |
//! | `[1, 7]` | fence to those `projects.id` values |
//!
//! Collapsing the first two loses the security property: `None` must serve any
//! session and `[]` must serve none, and both are "no ids". `Option<Vec<i64>>`
//! carries the distinction in the type; a bare `Vec<i64>` would not, which is
//! why the signature is what it is.
//!
//! # The read-through cache is NOT ported — and it can change an answer
//!
//! `_CONTEXT_CACHE` memoises the full timeline on `(store_path, session_fk)`,
//! validated by a `(MAX(timestamp), COUNT(*))` signature over that session's
//! messages and deep-copied on read. The precedent is DIV-055: `/api/stats`'s
//! memo was recorded as not-ported rather than reproduced blind, because a
//! self-invalidating memo is a latency device and reproducing it is a memory
//! decision this campaign has not measured.
//!
//! The same call is made here, with one thing said out loud that DIV-055 could
//! not say: this signature does **not** cover an in-place edit. A backfill that
//! rewrites `raw_json` or `content_text` without changing the row count or the
//! newest timestamp leaves a warm entry serving stale previews and stale token
//! counts for the life of the process — and the dict has no size bound at all
//! (unlike `_project_stats_cached`'s 8-entry LRU), so every session ever
//! scrubbed is retained forever. Both are recorded as **DIV-105** rather than
//! reproduced. The port recomputes per request, which is slower and cannot be
//! stale.
//!
//! # `schema.apply(conn)` is not ported either — DIV-106
//!
//! The Python handler runs a **migration** on every request, to guard the
//! fresh-install case where a request beats the lifespan hook. The port never
//! migrates a store. That is payload-neutral and it was checked rather than
//! assumed: on a store with no tables at all, Python's post-migration lookup
//! finds no session and the port's lookup raises-and-is-swallowed, and both
//! arrive at the identical `session not found in store: …` body — pinned here
//! by `a_store_that_never_had_a_schema_still_answers_two_hundred`.

use axum::Router;
use axum::extract::{Path as PathParam, RawQuery, State};
use axum::http::StatusCode;
use axum::routing::get;
use rusqlite::Connection;
use serde_json::{Map, Value};

use crate::json::{HandlerResult, HttpError, JsonBody, join_failure};
use crate::qs::Query;
use crate::services::context_replay as service;
use crate::services::context_replay::py_strip;
use crate::state::AppState;

/// Mount this module's endpoint onto `router`.
///
/// axum 0.8 spells a path parameter `{session_id}` — the same braces FastAPI
/// uses — so the route string is a copy, not a transliteration. Like
/// starlette's default `str` converter, `{session_id}` matches one segment and
/// stops at a `/`.
pub fn register(router: Router<AppState>) -> Router<AppState> {
    router.route("/api/context-replay/{session_id}", get(get_context_replay))
}

/// `GET /api/context-replay/{session_id}`.
///
/// ```text
/// {"session_id", "at_seq", "message_count", "total_tokens",
///  "events": [{"seq", "role", "content_preview", "tokens",
///              "cumulative_tokens", "tool_calls"}...],
///  "warnings": [...]}
/// ```
///
/// Always `200` — see the module docs. The one non-200 is FastAPI's own `422`
/// for a query parameter that will not coerce, which validation raises *before*
/// the handler body runs and which the body therefore cannot swallow.
async fn get_context_replay(
    State(state): State<AppState>,
    PathParam(session_id): PathParam<String>,
    RawQuery(raw): RawQuery,
) -> HandlerResult {
    let query = Query::parse(raw.as_deref().unwrap_or_default());
    // `at: int | None = Query(None)`. Python then re-checks
    // `isinstance(at, int) and not isinstance(at, bool)` — defensiveness aimed
    // at a DIRECT call in a test, where `at` is still the `Query` sentinel.
    // Over HTTP pydantic has already coerced or rejected, so the parse IS the
    // check. Measured against fastapi 0.141.1 / pydantic 2.13.4: `?at=abc`,
    // `?at=`, `?at=5.5`, `?at=true` and `?at=0x10` are all `422 int_parsing`;
    // `?at=+5` and `?at=%20 7%20` are 200; `?at=1&at=2` takes the LAST.
    let at_seq = match query.opt_int("at") {
        Ok(value) => value,
        Err(err) => {
            return Ok(JsonBody::with_status(
                StatusCode::UNPROCESSABLE_ENTITY,
                validation_detail(&err),
            ));
        }
    };
    // `project_str = project if isinstance(project, str) else None` — over HTTP
    // an absent parameter is `None` and a present one is always a string, so
    // the guard reduces to "was it sent". `?project=` sends the empty string,
    // which is a real value here and is NOT the same as absent.
    let project = query.get("project").map(str::to_owned);
    let log_path = query.get("log_path").map(str::to_owned);
    // `deps.current_log_path`, read once on the request thread rather than
    // inside the blocking closure — same value either way, one lock fewer.
    let current_log_path = state.current_project().log_path;

    let worker = state.clone();
    tokio::task::spawn_blocking(move || {
        replay(
            &worker,
            &session_id,
            at_seq,
            project.as_deref(),
            log_path.as_deref(),
            current_log_path.as_deref(),
        )
    })
    .await
    .map_err(|err| join_failure(&err))?
}

/// The handler body, off the event loop: sqlite from first line to last.
///
/// # Errors
/// Only when the store will not open. Python has no `try` around
/// `db.connect(deps.store_path)` either, so an unopenable store is a 500 on
/// both sides; the body differs (starlette's `ServerErrorMiddleware` writes
/// `text/plain`), which is unreachable on any store the differ runs against and
/// is noted rather than emulated.
fn replay(
    state: &AppState,
    session_id: &str,
    at_seq: Option<i64>,
    project: Option<&str>,
    log_path: Option<&str>,
    current_log_path: Option<&str>,
) -> HandlerResult {
    let conn = state.connect().map_err(|err| {
        HttpError::new(StatusCode::INTERNAL_SERVER_ERROR, format!("store: {err}"))
    })?;

    // `schema.apply(conn)` is deliberately absent — DIV-106, module docs.

    let Some((_session_fk, sid, project_id)) = resolve_session_row(&conn, session_id) else {
        // Unknown session — advisory empty-but-valid, NOT a 404. Note this leg
        // skips the cache and calls the builder on an id that resolves to
        // nothing, which is why that function has to be miss-safe.
        let full = service::build_context_timeline(&conn, session_id);
        return Ok(JsonBody::ok(service::slice_context_timeline(&full, at_seq)));
    };

    let scope_ids = scope_project_ids(&conn, project, log_path, current_log_path);
    // `if scope_ids is not None and project_id not in scope_ids` — the `None`
    // leg is "no fence", NOT "empty fence". See the module docs.
    if scope_ids.is_some_and(|ids| !ids.contains(&project_id)) {
        // Cross-project fence — never serve another project's transcript.
        return Ok(JsonBody::ok(service::empty_context(
            &sid,
            at_seq,
            &[format!("session {sid} is outside the active project scope")],
        )));
    }

    // `_build_timeline_cached(...)` in Python; an uncached rebuild here —
    // DIV-105.
    let full = service::build_context_timeline(&conn, &sid);
    Ok(JsonBody::ok(service::slice_context_timeline(&full, at_seq)))
}

/// `_resolve_session_row` — `session_id` → `(session_fk, session_id, project_id)`.
///
/// The route's own resolver, distinct from the service's only in also returning
/// `project_id`, which the fence needs. `ORDER BY last_ts DESC NULLS LAST, id
/// DESC LIMIT 1` is the reference statement verbatim (LAW 5): `NULLS LAST` is
/// load-bearing, because SQLite's default under `DESC` puts NULLs *first* and a
/// session that never recorded a `last_ts` would then outrank every session
/// that did.
///
/// `except Exception: return None` — a bare `Exception`, wider than the
/// service's `sqlite3.Error`, though every driver error derives from that
/// anyway. The column conversions are swallowed here too; Python runs them
/// outside its `try` and would 500 on a NULL, which `NOT NULL` makes
/// unreachable.
fn resolve_session_row(conn: &Connection, session_id: &str) -> Option<(i64, String, i64)> {
    conn.query_row(
        "SELECT id, session_id, project_id FROM sessions WHERE session_id = ? \
         ORDER BY last_ts DESC NULLS LAST, id DESC LIMIT 1",
        [session_id],
        |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
            ))
        },
    )
    .ok()
}

/// `_scope_project_ids` — the active scope as `projects.id` values, or `None`.
///
/// Precedence: an explicit `?project=<slug>` wins; else `?log_path=`'s
/// basename; else the server's current project. All three tests are Python
/// *truthiness* on the stripped string, so `?project=%20` is blank and falls
/// through to the next leg rather than becoming a slug of one space.
///
/// The three return shapes are the security contract — see the module docs.
fn scope_project_ids(
    conn: &Connection,
    project: Option<&str>,
    log_path: Option<&str>,
    current_log_path: Option<&str>,
) -> Option<Vec<i64>> {
    let slug: Option<String> = match project.map(py_strip).filter(|slug| !slug.is_empty()) {
        // `slug = project.strip()` — the STRIPPED value becomes the slug.
        Some(slug) => Some(slug.to_owned()),
        None => {
            // `path = log_path if isinstance(log_path, str) and log_path.strip()
            //         else deps.current_log_path`.
            let path = match log_path.filter(|candidate| !py_strip(candidate).is_empty()) {
                Some(path) => Some(path),
                None => current_log_path,
            };
            // `if path:` — an EMPTY current log path is falsy and yields no
            // scope at all, which is the "provider with no on-disk log dir"
            // case `state::CurrentProject` keeps a `Some("")` for.
            path.filter(|path| !path.is_empty()).map(path_name)
        }
    };
    // `if not slug: return None` — including a slug that stripped to nothing.
    let slug = slug.filter(|slug| !slug.is_empty())?;

    match project_ids_for_slug(conn, &slug) {
        Ok(ids) => Some(ids),
        // `except Exception: return []` — a bad store fences EVERYTHING. Note
        // the asymmetry with the resolver above, which swallows into `None`:
        // here the safe default is "serve nothing", there it is "unknown
        // session". Both are the reference's choice.
        Err(_) => Some(Vec::new()),
    }
}

/// `conn.execute("SELECT id FROM projects WHERE slug = ?")`.
///
/// A list and not a scalar: the schema's `UNIQUE(provider, slug)` means one
/// slug can name several projects, one per provider, and the fence must admit
/// all of them.
fn project_ids_for_slug(conn: &Connection, slug: &str) -> rusqlite::Result<Vec<i64>> {
    let mut stmt = conn.prepare("SELECT id FROM projects WHERE slug = ?")?;
    let rows = stmt.query_map([slug], |row| row.get::<_, i64>(0))?;
    rows.collect()
}

/// `Path(path).name`.
fn path_name(path: &str) -> String {
    std::path::Path::new(path)
        .file_name()
        .map_or_else(String::new, |name| name.to_string_lossy().into_owned())
}

/// pydantic's `422` body — see DIV-053.
///
/// `at` is the only coerced parameter this module has, so `int_parsing` is the
/// only `type` reachable. Measured byte-identical against fastapi 0.141.1 for
/// `?at=abc` and `?at=`.
fn validation_detail(err: &crate::qs::QueryError) -> Value {
    let mut entry = Map::new();
    entry.insert("type".to_owned(), Value::from(err.kind));
    entry.insert(
        "loc".to_owned(),
        Value::Array(vec![Value::from("query"), Value::from(err.field.clone())]),
    );
    entry.insert(
        "msg".to_owned(),
        Value::from("Input should be a valid integer, unable to parse string as an integer"),
    );
    entry.insert("input".to_owned(), Value::from(err.input.clone()));
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
    use crate::state::{Config, CurrentProject};
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt as _;

    /// A scratch `STACKUNDERFLOW_HOME` that cleans itself up.
    struct Scratch(std::path::PathBuf);

    impl Scratch {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "stax-ctxreplay-{tag}-{}-{}",
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

    /// Two projects, two sessions, one in each. Enough to exercise the fence in
    /// all three of its shapes.
    fn seeded_state(scratch: &Scratch) -> AppState {
        let store = scratch.0.join("store.db");
        let conn = Connection::open(&store).expect("open");
        conn.execute_batch(
            r#"CREATE TABLE projects (id INTEGER PRIMARY KEY, slug TEXT);
             CREATE TABLE sessions (
                 id INTEGER PRIMARY KEY, project_id INTEGER NOT NULL,
                 session_id TEXT NOT NULL, last_ts TEXT);
             CREATE TABLE messages (
                 id INTEGER PRIMARY KEY, session_fk INTEGER NOT NULL, seq INTEGER NOT NULL,
                 timestamp TEXT, role TEXT NOT NULL DEFAULT '',
                 content_text TEXT NOT NULL DEFAULT '',
                 tools_json TEXT NOT NULL DEFAULT '[]', raw_json TEXT NOT NULL DEFAULT '');
             INSERT INTO projects (id, slug) VALUES (1, '-p-one'), (2, '-p-two');
             INSERT INTO sessions (id, project_id, session_id, last_ts) VALUES
                 (10, 1, 'in-one', '2026-01-01T00:00:00Z'),
                 (11, 2, 'in-two', '2026-01-02T00:00:00Z');
             INSERT INTO messages (session_fk, seq, role, content_text) VALUES
                 (10, 1, 'user', 'first turn'),
                 (10, 2, 'assistant', 'second turn'),
                 (11, 1, 'user', 'other project');"#,
        )
        .expect("seed");
        drop(conn);
        AppState::new(store, scratch.0.clone(), Config::default())
    }

    /// Drive the mounted route in-process — no port, so nothing can collide
    /// with the reserved `:8095` / `:8096`.
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
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .expect("body");
        (status, String::from_utf8(bytes.to_vec()).expect("utf-8"))
    }

    #[tokio::test]
    async fn the_happy_path_is_the_whole_session_with_at_seq_left_null() {
        let scratch = Scratch::new("happy");
        let state = seeded_state(&scratch);
        let (status, body) = call(&state, "/api/context-replay/in-one").await;
        assert_eq!(status, StatusCode::OK);
        // Byte-for-byte the reference's `json.dumps(..., ensure_ascii=False,
        // separators=(",", ":"))` for this fixture.
        assert_eq!(
            body,
            r#"{"session_id":"in-one","at_seq":null,"message_count":2,"total_tokens":6,"events":[{"seq":1,"role":"user","content_preview":"first turn","tokens":3,"cumulative_tokens":3,"tool_calls":[]},{"seq":2,"role":"assistant","content_preview":"second turn","tokens":3,"cumulative_tokens":6,"tool_calls":[]}],"warnings":[]}"#
        );
    }

    #[tokio::test]
    async fn the_at_cutoff_re_slices_and_the_echoed_at_seq_is_the_requested_one() {
        let scratch = Scratch::new("at");
        let state = seeded_state(&scratch);
        let (status, body) = call(&state, "/api/context-replay/in-one?at=1").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body,
            r#"{"session_id":"in-one","at_seq":1,"message_count":1,"total_tokens":3,"events":[{"seq":1,"role":"user","content_preview":"first turn","tokens":3,"cumulative_tokens":3,"tool_calls":[]}],"warnings":[]}"#
        );
    }

    #[tokio::test]
    async fn an_unknown_session_is_a_two_hundred_with_a_warning_and_never_a_four_oh_four() {
        let scratch = Scratch::new("unknown");
        let state = seeded_state(&scratch);
        let (status, body) = call(&state, "/api/context-replay/ghost?at=5").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body,
            r#"{"session_id":"ghost","at_seq":5,"message_count":0,"total_tokens":0,"events":[],"warnings":["session not found in store: ghost"]}"#
        );
    }

    #[tokio::test]
    async fn a_percent_encoded_session_id_is_decoded_and_echoed_as_raw_utf_eight() {
        // Two independent contracts meet on one line: axum must percent-decode
        // the path parameter the way starlette does (measured: `caf%C3%A9`
        // reaches the handler as `café`), and the body writer must ship the
        // three raw bytes because starlette's `JSONResponse` sets
        // `ensure_ascii=False`. The CLI writer would emit `café` here and
        // the differ would fail on a single project name.
        let scratch = Scratch::new("utf8");
        let state = seeded_state(&scratch);
        let (status, body) = call(&state, "/api/context-replay/caf%C3%A9").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.starts_with(r#"{"session_id":"café","#), "{body}");
        assert!(body.contains("session not found in store: café"), "{body}");
    }

    #[tokio::test]
    async fn a_session_in_another_project_is_fenced_to_the_empty_shape_with_the_scope_warning() {
        let scratch = Scratch::new("fence");
        let state = seeded_state(&scratch);
        // A scope is active, and `in-two` is not in it.
        let (status, body) = call(&state, "/api/context-replay/in-two?project=-p-one").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body,
            r#"{"session_id":"in-two","at_seq":null,"message_count":0,"total_tokens":0,"events":[],"warnings":["session in-two is outside the active project scope"]}"#
        );
        // The fence is not sliced, so `at` still lands in the echoed `at_seq`.
        let (_, body) = call(&state, "/api/context-replay/in-two?project=-p-one&at=1").await;
        assert!(body.contains(r#""at_seq":1"#), "{body}");
    }

    #[tokio::test]
    async fn no_scope_at_all_serves_any_session_in_the_store() {
        let scratch = Scratch::new("noscope");
        let state = seeded_state(&scratch);
        // Nothing selected, no query scope: `_scope_project_ids` is None and
        // the fence is skipped entirely. Collapsing None into [] would fence
        // this and the endpoint would answer nothing, ever.
        let (status, body) = call(&state, "/api/context-replay/in-two").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains(r#""message_count":1"#), "{body}");
        assert!(body.contains(r#""warnings":[]"#), "{body}");
    }

    #[tokio::test]
    async fn a_slug_that_names_no_project_is_an_empty_scope_and_fences_everything() {
        let scratch = Scratch::new("emptyscope");
        let state = seeded_state(&scratch);
        let (status, body) = call(&state, "/api/context-replay/in-one?project=-nope").await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            body.contains("is outside the active project scope"),
            "{body}"
        );
    }

    #[tokio::test]
    async fn the_current_project_fences_when_no_query_scope_is_given() {
        let scratch = Scratch::new("current");
        let state = seeded_state(&scratch);
        state.set_current_project(CurrentProject {
            project_path: Some("/x/-p-one".to_owned()),
            log_path: Some("/logs/-p-one".to_owned()),
        });
        // `Path(log_path).name` is the slug, so `-p-one` is in scope…
        let (_, body) = call(&state, "/api/context-replay/in-one").await;
        assert!(body.contains(r#""message_count":2"#), "{body}");
        // …and the other project's session is fenced.
        let (_, body) = call(&state, "/api/context-replay/in-two").await;
        assert!(
            body.contains("is outside the active project scope"),
            "{body}"
        );
    }

    #[tokio::test]
    async fn an_explicit_project_beats_a_log_path_which_beats_the_current_project() {
        let scratch = Scratch::new("precedence");
        let state = seeded_state(&scratch);
        state.set_current_project(CurrentProject {
            project_path: Some("/x/-p-one".to_owned()),
            log_path: Some("/logs/-p-one".to_owned()),
        });
        // `log_path` overrides the current project…
        let (_, body) = call(&state, "/api/context-replay/in-two?log_path=/logs/-p-two").await;
        assert!(body.contains(r#""message_count":1"#), "{body}");
        // …and an explicit `project` overrides the `log_path`.
        let (_, body) = call(
            &state,
            "/api/context-replay/in-two?project=-p-one&log_path=/logs/-p-two",
        )
        .await;
        assert!(
            body.contains("is outside the active project scope"),
            "{body}"
        );
        // A blank `project` is falsy and falls through to `log_path`.
        let (_, body) = call(
            &state,
            "/api/context-replay/in-two?project=%20&log_path=/logs/-p-two",
        )
        .await;
        assert!(body.contains(r#""message_count":1"#), "{body}");
    }

    #[tokio::test]
    async fn a_non_integer_at_is_fastapis_four_twenty_two_and_the_handler_never_runs() {
        let scratch = Scratch::new("badat");
        let state = seeded_state(&scratch);
        let (status, body) = call(&state, "/api/context-replay/in-one?at=abc").await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(
            body,
            r#"{"detail":[{"type":"int_parsing","loc":["query","at"],"msg":"Input should be a valid integer, unable to parse string as an integer","input":"abc"}]}"#
        );
        // An EMPTY `?at=` is the same 422, with `input` the empty string.
        let (status, body) = call(&state, "/api/context-replay/in-one?at=").await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(body.contains(r#""input":"""#), "{body}");
    }

    #[tokio::test]
    async fn a_repeated_at_takes_the_last_and_whitespace_and_a_leading_plus_coerce() {
        let scratch = Scratch::new("coerce");
        let state = seeded_state(&scratch);
        // starlette's `QueryParams._dict` keeps the LAST occurrence.
        let (_, body) = call(&state, "/api/context-replay/in-one?at=2&at=1").await;
        assert!(body.contains(r#""at_seq":1"#), "{body}");
        // pydantic strips whitespace and accepts a leading `+`.
        let (status, body) = call(&state, "/api/context-replay/in-one?at=%2B1").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains(r#""at_seq":1"#), "{body}");
    }

    #[tokio::test]
    async fn a_negative_at_keeps_nothing_and_is_still_a_two_hundred() {
        let scratch = Scratch::new("negat");
        let state = seeded_state(&scratch);
        let (status, body) = call(&state, "/api/context-replay/in-one?at=-1").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body,
            r#"{"session_id":"in-one","at_seq":-1,"message_count":0,"total_tokens":0,"events":[],"warnings":[]}"#
        );
    }

    #[tokio::test]
    async fn a_missing_session_segment_is_the_routers_four_oh_four_not_an_empty_id() {
        let scratch = Scratch::new("nosegment");
        let state = seeded_state(&scratch);
        // `{session_id}` must not match an empty segment; if it did, the empty
        // id would reach the handler and answer 200 where FastAPI answers 404.
        let (status, _) = call(&state, "/api/context-replay/").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn a_store_that_never_had_a_schema_still_answers_two_hundred() {
        // The DIV-106 guarantee at the HTTP boundary: no `schema.apply`, no
        // tables, and the body is still the advisory empty shape.
        let scratch = Scratch::new("noschema");
        let state = AppState::new(
            scratch.0.join("store.db"),
            scratch.0.clone(),
            Config::default(),
        );
        let (status, body) = call(&state, "/api/context-replay/anything").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body,
            r#"{"session_id":"anything","at_seq":null,"message_count":0,"total_tokens":0,"events":[],"warnings":["session not found in store: anything"]}"#
        );
    }

    #[test]
    fn the_scope_resolver_keeps_none_and_the_empty_list_distinct() {
        let conn = Connection::open_in_memory().expect("open");
        conn.execute_batch(
            "CREATE TABLE projects (id INTEGER PRIMARY KEY, slug TEXT);
             INSERT INTO projects (id, slug) VALUES (1, '-p-one'), (2, '-p-one');",
        )
        .expect("seed");
        // No scope of any kind: `None`, which fences nothing.
        assert_eq!(scope_project_ids(&conn, None, None, None), None);
        // A blank current log path is falsy — still `None`, not `[]`.
        assert_eq!(scope_project_ids(&conn, None, None, Some("")), None);
        // A slug with no rows: `[]`, which fences everything.
        assert_eq!(
            scope_project_ids(&conn, Some("-nope"), None, None),
            Some(Vec::new())
        );
        // One slug, two providers, two ids — the fence admits both.
        assert_eq!(
            scope_project_ids(&conn, Some("-p-one"), None, None),
            Some(vec![1, 2])
        );
    }

    #[test]
    fn a_broken_store_fences_everything_rather_than_opening_the_gate() {
        // `except Exception: return []`. The safe default under a bad store is
        // "serve nothing", and getting this backwards would leak transcripts.
        let conn = Connection::open_in_memory().expect("open");
        assert_eq!(
            scope_project_ids(&conn, Some("-p-one"), None, None),
            Some(Vec::new())
        );
    }
}
