//! `routes/etl.py` — 2 endpoints, wave 5 (batch E). **DIV-139 CLEARED.**
//!
//! | Item | Method | FastAPI path | axum path | State |
//! |---|---|---|---|---|
//! | `RS-5-071` | `GET ` | `/api/etl/status  ` | `/api/etl/status`   | ported |
//! | `RS-5-072` | `POST` | `/api/etl/backfill` | `/api/etl/backfill` | ported — **no case row** |
//!
//! Both halves were deferred out of batch D, for different reasons. Both reasons
//! were about *where the work lives*, not about whether it could be ported.
//!
//! **`GET /api/etl/status` is four lines over a 566-line assembler.** That
//! assembler is [`crate::services::etl_status`], because `stax etl
//! status` (the CLI verb, wave 8) calls the same function and a transliteration
//! into this file would fork it — the batch-C ruling for every thin-wrapper
//! route. The route itself opens a connection, applies the schema, and returns
//! the dict.
//!
//! **`POST /api/etl/backfill` is a writer with a process-local lock, and it has
//! NO CASE ROW.** Law 4: `!` suppresses the verdict, never the request
//! (DIV-059), and this request rebuilds the marts on whatever home the harness
//! points at — the DIV-078 hazard exactly, the one that cost a 520 MB search
//! index rebuild. Its `409` leg additionally needs a job already in flight,
//! which is state a shared differ cannot arrange on two servers at once. Its
//! verification is the isolated procedure in `rust/ETL-BACKFILL-DIFFER.md`, on
//! two separate scratch state copies and its own ports — the same shape
//! `rust/REFRESH-DIFFER.md` established for `POST /api/refresh`, for the same
//! four reasons.
//!
//! # `schema.apply` is not ported, and the difference is a WRITE
//!
//! Python's handler calls `schema.apply(conn)` before assembling, "so the etl
//! tables exist on a fresh-install machine where the server hasn't yet booted to
//! install them". On an already-current store that is one `PRAGMA user_version`
//! read and nothing else. On a store behind the migration chain it is a **write
//! performed by a GET**. The migration runner is RS-0-025 and is unported, so
//! this handler does not run it; the assembler's per-block `sqlite_master`
//! guards mean a store missing the tables answers zeros instead. The wire
//! difference is confined to a store that is behind, where Python creates the
//! tables and then reports the same zeros this reports without creating them.
//! Recorded as a numbered finding rather than closed silently.
//!
//! # The background task
//!
//! FastAPI's `BackgroundTasks` runs the task **after the response is flushed**,
//! in the same worker thread. axum has no equivalent, so the work is handed to
//! `tokio::task::spawn_blocking` — which starts it *slightly* earlier, before
//! the response bytes are on the wire rather than after. Nothing observable
//! turns on the ordering: the job slot is claimed synchronously inside the
//! handler on both sides, so a second POST racing the first gets its `409` from
//! `start_job` either way, and `/api/etl/status` cannot report on a job it has
//! not been told about. Named because "equivalent" is a claim and this one has a
//! measurable edge case (a status poll issued in the microseconds between the
//! response and the task start would see `current_job` non-null on both sides —
//! the slot, not the task, is what it reads).

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, post};
use serde_json::{Map, Value};
use stax_core::queries::pytime;

use crate::json::{HandlerResult, HttpError, JsonBody, join_failure};
use crate::services::etl_backfill::{
    BackfillInProgress, backfill, complete_job, get_current_job, get_last_job, start_job,
};
use crate::services::etl_status::assemble_status;
use crate::state::AppState;

/// Mount this module's endpoints onto `router`.
pub fn register(router: Router<AppState>) -> Router<AppState> {
    router
        .route("/api/etl/status", get(get_etl_status))
        .route("/api/etl/backfill", post(post_etl_backfill))
}

// ── GET /api/etl/status ──────────────────────────────────────────────────────

/// `GET /api/etl/status` — RS-5-071. The live ETL snapshot.
///
/// `STACKUNDERFLOW_DISABLE_WATCHER` is read *here*, per request, because Python
/// reads `os.environ` per request in `_watcher_env_disabled` — it is not one of
/// the settings `Config` resolves once at startup, and a server whose operator
/// exported the variable after boot would report the new value on the reference.
/// Reading (unlike `set_var`) is safe and needs no `unsafe`.
///
/// The app directory is `store_path.parent()`: Python's `deps.store_path` is
/// `settings.app_dir() / "store.db"` and `etl/lock.py`'s `DEFAULT_LOCK_PATH` is
/// `settings.app_dir() / "server.lock"`, so the two are siblings by
/// construction.
async fn get_etl_status(State(state): State<AppState>) -> HandlerResult {
    let worker = state.clone();
    let disable_watcher = std::env::var("STACKUNDERFLOW_DISABLE_WATCHER").ok();
    let payload = tokio::task::spawn_blocking(move || {
        let conn = worker.connect().map_err(|err| any_500(&err))?;
        let app_dir = worker
            .store_path()
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."))
            .to_path_buf();
        // The two job slots are the server's, and only the server has any:
        // `assemble_status` takes them as parameters now (it moved to
        // `stax-reports` so `stax etl status` could call it, and a
        // CLI process passes `None, None`). `get_last_job`'s lazy, DESTRUCTIVE
        // TTL expiry therefore fires HERE, on this read, exactly as it fired
        // inside the assembler before — same clock, same call, same side
        // effect. Read before the blocking work so the two slots are sampled at
        // the same instant Python samples them.
        let now = pytime::now_micros();
        let current_job = get_current_job().map(|job| job.current_value());
        let last_job = get_last_job(now).map(|job| job.last_value());
        assemble_status(
            &conn,
            &app_dir,
            disable_watcher.as_deref(),
            current_job,
            last_job,
        )
        .map_err(sql_500)
    })
    .await
    .map_err(|err| join_failure(&err))??;
    Ok(JsonBody::ok(payload))
}

// ── POST /api/etl/backfill ───────────────────────────────────────────────────

/// `POST /api/etl/backfill` — RS-5-072. Schedule a background backfill run.
///
/// The body parameter is `body: dict | None = None`, which is **not**
/// `/api/refresh`'s `request: dict`. The default makes the body optional and the
/// `| None` makes an explicit JSON `null` a legal value, so four inputs reach the
/// handler with `body = None` — absent, empty, `null`, and `{}` (the last with an
/// empty dict) — and only a body that is valid JSON of the wrong *shape*, or not
/// JSON at all, is rejected before the handler runs.
///
/// `202` carries `{"job_id", "started_at"}`; `409` carries `{"error":
/// "backfill_in_progress", "job_id"}` naming the run already in flight.
async fn post_etl_backfill(
    State(state): State<AppState>,
    body: axum::body::Bytes,
) -> HandlerResult {
    // Taken as raw bytes rather than through `axum::Json`, which would need the
    // `json` feature and would still not produce pydantic's error shape. FastAPI
    // validates before the handler runs, so a rejection here never reaches
    // `start_job` and never schedules anything — which is what makes the 422
    // probes in `ETL-BACKFILL-DIFFER.md` safe to issue against a live server.
    // `body: dict | None = None` — the OPTIONAL member of DIV-367's class: an
    // absent body and a literal `null` are both legal and both mean `None`, and
    // only valid JSON of the wrong shape (`dict_type`) or no JSON at all
    // (`json_invalid`) is rejected. `missing` therefore does not exist on this
    // endpoint, which is the whole difference the shared helper's `optional`
    // flag encodes.
    let parsed = match crate::json::optional_dict_body(&body) {
        Ok(parsed) => parsed,
        Err(rejection) => return Ok(rejection),
    };

    // `bool((body or {}).get("force", False))` — Python truthiness over whatever
    // the key holds, so `"force": "no"` is TRUE (a non-empty string) and
    // `"force": 0` is false. A `bool()` cast, not a type check.
    let force = parsed
        .as_ref()
        .and_then(|map| map.get("force"))
        .is_some_and(py_truthy);

    let job = match start_job(force, pytime::now_micros()) {
        Ok(job) => job,
        Err(BackfillInProgress { current_job }) => {
            let mut out = Map::new();
            out.insert("error".to_owned(), Value::from("backfill_in_progress"));
            out.insert("job_id".to_owned(), Value::from(current_job.job_id));
            return Ok(JsonBody::with_status(
                StatusCode::CONFLICT,
                Value::Object(out),
            ));
        }
    };

    let worker = state.clone();
    let job_id = job.job_id.clone();
    tokio::task::spawn_blocking(move || run_backfill_in_background(&worker, &job_id, force));

    let mut out = Map::new();
    out.insert("job_id".to_owned(), Value::from(job.job_id));
    out.insert("started_at".to_owned(), Value::from(job.started_at));
    Ok(JsonBody::with_status(
        StatusCode::ACCEPTED,
        Value::Object(out),
    ))
}

/// `bool(x)` for the JSON values a request body can carry.
///
/// `{}` and `[]` and `""` and `0` and `0.0` and `null` are falsy; everything else
/// is truthy. `0.0` includes `-0.0`, which Python also calls falsy.
fn py_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|x| x != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(items) => !items.is_empty(),
        Value::Object(map) => !map.is_empty(),
    }
}

/// `_run_backfill_in_background` — the worker entry point.
///
/// Owns the connection lifecycle and **always** releases the slot, even when the
/// orchestrator fails. Errors are logged in Python and are unreportable here for
/// the same reason: the `202` went out long ago. The next `/api/etl/status` sees
/// the cleared slot and, for thirty seconds, the `last_job` block with
/// `status: "failed"` and the stringified error.
///
/// The `finally` nesting matters and is reproduced: Python closes the connection
/// inside a `try` whose `finally` calls `complete_job`, so a failure to close
/// still releases the slot. Rust drops the connection at scope exit, before
/// `complete_job` is reached, and a `Drop` that fails cannot unwind — so the
/// release is unconditional here by construction rather than by nesting.
fn run_backfill_in_background(state: &AppState, job_id: &str, force: bool) {
    let outcome = (|| -> anyhow::Result<()> {
        let conn = state.connect()?;
        // `schema.apply(conn)` — unported (RS-0-025); see the module docs.
        let engine = crate::pricing::engine(&conn, state.package_dir())?;
        let ctx = stax_etl::normalize::NormalizeContext::new(engine);
        backfill(
            &conn,
            &ctx,
            force,
            &pytime::isoformat_utc(pytime::now_micros()),
        )?;
        Ok(())
    })();

    match outcome {
        Ok(()) => complete_job(job_id, "complete", None, pytime::now_micros()),
        // `str(err)`. `anyhow`'s `Display` is the outermost message only, which
        // is the same shape `str(exc)` produces for a Python exception; the
        // causal chain is `{err:#}` and Python has no equivalent of it here.
        Err(err) => complete_job(
            job_id,
            "failed",
            Some(err.to_string()),
            pytime::now_micros(),
        ),
    }
}

// The 422s for this endpoint's `body: dict | None = None` live in
// [`crate::json::optional_dict_body`] — DIV-367's shared extractor.
//
// `ETL-BACKFILL-DIFFER.md` step 2 had already MEASURED both reachable shapes
// here (five byte-identical 422s, 2026-07-31), which is why this module's copy
// was the one worth keeping when the class was collapsed: it was evidence, not
// a transcription. What the shared version adds is the `null` body, which the
// isolated differ had not sent and which the required members answer
// differently (`missing`, not `dict_type`).

fn sql_500(err: rusqlite::Error) -> HttpError {
    HttpError::new(StatusCode::INTERNAL_SERVER_ERROR, err.to_string())
}

fn any_500(err: &anyhow::Error) -> HttpError {
    HttpError::new(StatusCode::INTERNAL_SERVER_ERROR, err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt as _;

    use crate::services::etl_backfill::{reset_for_tests, test_lock};

    /// A router over a real, empty store on disk. `AppState::connect` opens
    /// read-write through the live-dataset guard, so an in-memory connection is
    /// not an option.
    fn app(tag: &str) -> (Router, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "stax-etl-route-{tag}-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        ));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        let store = dir.join("store.db");
        {
            let conn = rusqlite::Connection::open(&store).expect("store");
            conn.execute_batch(crate::services::etl_backfill::testdb::SCHEMA)
                .expect("schema");
        }
        let state = AppState::new(store, dir.clone(), crate::state::Config::default());
        (register(Router::new()).with_state(state), dir)
    }

    async fn send(router: &Router, request: Request<Body>) -> (StatusCode, String) {
        let response = router
            .clone()
            .oneshot(request)
            .await
            .expect("router answers");
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .expect("body");
        (status, String::from_utf8_lossy(&bytes).into_owned())
    }

    fn post(body: &str) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri("/api/etl/backfill")
            .header("content-type", "application/json")
            .body(Body::from(body.to_owned()))
            .expect("request")
    }

    // The job slot is a process global; `test_lock` serialises the tests that
    // touch it. The guard is a `std::sync::MutexGuard` held across `.await`,
    // which `#[tokio::test]` drives on a single-threaded runtime — nothing can
    // be scheduled onto another thread while it is held, so the lint's hazard
    // (blocking an executor thread) cannot arise here.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn status_answers_a_complete_payload_on_an_empty_store() {
        let _guard = test_lock();
        reset_for_tests();
        let (router, dir) = app("status");
        let (status, body) = send(
            &router,
            Request::builder()
                .uri("/api/etl/status")
                .body(Body::empty())
                .expect("request"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let parsed: Value = serde_json::from_str(&body).expect("json");
        assert_eq!(parsed["health"], Value::from("live"));
        assert_eq!(parsed["lag_seconds"], Value::from(0));
        assert_eq!(parsed["current_job"], Value::Null);
        assert_eq!(
            parsed["marts"].as_object().expect("marts").len(),
            5,
            "five names even on a store with eight mart tables"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The 202 → 409 → 202 cycle, driven through the router. The background task
    /// is spawned but the slot is claimed synchronously, so the second POST sees
    /// the conflict without any timing dependence.
    // The job slot is a process global; `test_lock` serialises the tests that
    // touch it. The guard is a `std::sync::MutexGuard` held across `.await`,
    // which `#[tokio::test]` drives on a single-threaded runtime — nothing can
    // be scheduled onto another thread while it is held, so the lint's hazard
    // (blocking an executor thread) cannot arise here.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn a_second_backfill_while_one_is_claimed_is_a_409_naming_the_first() {
        let _guard = test_lock();
        reset_for_tests();
        let (router, dir) = app("conflict");

        let (status, body) = send(&router, post(r#"{"force": false}"#)).await;
        assert_eq!(status, StatusCode::ACCEPTED);
        let first: Value = serde_json::from_str(&body).expect("json");
        let job_id = first["job_id"].as_str().expect("job_id").to_owned();
        assert_eq!(job_id.len(), 32);
        assert!(
            first["started_at"]
                .as_str()
                .expect("ts")
                .ends_with("+00:00")
        );
        assert_eq!(
            first.as_object().expect("obj").keys().collect::<Vec<_>>(),
            vec!["job_id", "started_at"]
        );

        // Claim the slot by hand so the conflict cannot race the background
        // task's completion — this is the state the 409 leg needs and the state
        // a shared differ cannot arrange.
        reset_for_tests();
        let held = start_job(true, 1_767_312_000_000_000).expect("claim");
        let (status, body) = send(&router, post("{}")).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(
            body,
            format!(
                r#"{{"error":"backfill_in_progress","job_id":"{}"}}"#,
                held.job_id
            )
        );

        reset_for_tests();
        let (status, _) = send(&router, post("{}")).await;
        assert_eq!(status, StatusCode::ACCEPTED, "the slot is free again");
        reset_for_tests();
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The four bodies that mean `force=false` and the two that are 422s.
    // The job slot is a process global; `test_lock` serialises the tests that
    // touch it. The guard is a `std::sync::MutexGuard` held across `.await`,
    // which `#[tokio::test]` drives on a single-threaded runtime — nothing can
    // be scheduled onto another thread while it is held, so the lint's hazard
    // (blocking an executor thread) cannot arise here.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn the_body_is_optional_and_nullable_but_not_a_list() {
        let _guard = test_lock();
        let (router, dir) = app("bodies");

        for accepted in ["", "null", "{}", r#"{"other": 1}"#] {
            reset_for_tests();
            let (status, _) = send(&router, post(accepted)).await;
            assert_eq!(status, StatusCode::ACCEPTED, "body {accepted:?}");
        }

        reset_for_tests();
        let (status, body) = send(&router, post("[]")).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(
            body,
            r#"{"detail":[{"type":"dict_type","loc":["body"],"msg":"Input should be a valid dictionary","input":[]}]}"#
        );

        let (status, body) = send(&router, post("nope")).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(body.contains(r#""type":"json_invalid""#), "{body}");
        assert!(body.contains(r#""input":{}"#), "{body}");

        reset_for_tests();
        let _ = std::fs::remove_dir_all(dir);
    }

    /// `bool(...)`, not a type check: a non-empty string is a truthy `force`.
    #[test]
    fn force_is_a_python_bool_cast() {
        assert!(py_truthy(&Value::Bool(true)));
        assert!(py_truthy(&Value::from("no")));
        assert!(py_truthy(&Value::from(1)));
        assert!(py_truthy(&serde_json::json!({"a": 1})));
        assert!(!py_truthy(&Value::Bool(false)));
        assert!(!py_truthy(&Value::from(0)));
        assert!(!py_truthy(&Value::from(0.0)));
        assert!(!py_truthy(&Value::from("")));
        assert!(!py_truthy(&Value::Null));
        assert!(!py_truthy(&serde_json::json!([])));
        assert!(!py_truthy(&serde_json::json!({})));
    }
}
