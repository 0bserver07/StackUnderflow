//! `routes/webhooks.py` — 3 endpoints, wave 5 (batch D).
//!
//! | Item | Method | FastAPI path | axum path | State |
//! |---|---|---|---|---|
//! | `RS-5-110` | `POST` | `/api/webhooks/github` | `/api/webhooks/github` | ported |
//! | `RS-5-111` | `POST` | `/api/webhooks/gitlab` | `/api/webhooks/gitlab` | ported |
//! | `RS-5-112` | `POST` | `/api/webhooks/ci    ` | `/api/webhooks/ci`     | ported |
//!
//! # The only routes in the tree that are opt-in at the environment
//!
//! Every one reads its signing secret from an env var and answers **503** when
//! it is unset — "the server is alive but the receiver is not configured". That
//! is the only leg the parity harness can reach, and it is the right one to
//! reach: it proves the gate fires before the body is read, before it is parsed,
//! and before anything touches the store. All three rows are green and
//! side-effect-free (DIV-059), because on an unconfigured host the handler
//! returns before it can do anything at all.
//!
//! The legs below it are ported and unit-tested rather than diffed: a case row
//! would need the harness to export a secret into *both* server processes, and
//! `endpoint-parity.sh` is shared ground no batch may re-shape. The
//! length-check-then-compare order is reproduced — Python bails on a length
//! mismatch *before* `compare_digest`, so a truncated signature never reaches
//! the constant-time path.
//!
//! # A third JSON layout, live on three endpoints
//!
//! Every success returns `Response(content=json.dumps(result),
//! media_type="application/json")` — a bare `Response`, **not** a
//! `JSONResponse`. So the body is `json.dumps`'s *default* layout:
//! `ensure_ascii=True` (the CLI's flag) with `(", ", ": ")` separators, which is
//! neither writer this crate already has.
//!
//! ```text
//! JSONResponse (pyjson::dumps_http)   {"status":"pong"}
//! agent_output (pyjson::dumps_pretty) {\n  "status": "pong"\n}
//! this module  (dumps_py_default)     {"status": "pong"}
//! ```
//!
//! [`dumps_py_default`] is that third layout. The `raw_json` column both upserts
//! persist uses the same writer, because it is the same `json.dumps(payload,
//! default=str)` call. The 503 / 403 legs are the exception: those
//! `raise HTTPException`, which FastAPI renders through a real `JSONResponse` —
//! so one endpoint answers in two different JSON layouts depending on which line
//! returns, and both are reproduced.
//!
//! # No new dependency
//!
//! HMAC-SHA256 is built on `stax_etl::stats::sha256`, the transcribed FIPS 180-4
//! digest wave 3 already put in the tree, rather than on `hmac` + `sha2`. That
//! keeps batch D's promise not to move the workspace lock, and it follows the
//! precedent that module set: "a shared workspace lock is a heavier thing to
//! move than a well-tested constant table". RFC 4231's vectors pin it.

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use rusqlite::Connection;
use serde_json::{Map, Value};

use crate::json::{HttpError, JSON_CONTENT_TYPE};
use crate::state::AppState;

/// `ENV_GITHUB_SECRET`.
const ENV_GITHUB_SECRET: &str = "STACKUNDERFLOW_GITHUB_WEBHOOK_SECRET";
/// `ENV_GITLAB_SECRET`.
const ENV_GITLAB_SECRET: &str = "STACKUNDERFLOW_GITLAB_WEBHOOK_SECRET";
/// `ENV_CI_SECRET`.
const ENV_CI_SECRET: &str = "STACKUNDERFLOW_CI_WEBHOOK_SECRET";

/// Mount this module's endpoints onto `router`.
pub fn register(router: Router<AppState>) -> Router<AppState> {
    router
        .route("/api/webhooks/github", post(github_webhook))
        .route("/api/webhooks/gitlab", post(gitlab_webhook))
        .route("/api/webhooks/ci", post(ci_webhook))
}

// ── signature validation ─────────────────────────────────────────────────────

/// `_require_secret` — the env var, or a 503 naming it.
fn require_secret(env_var: &str) -> Result<String, HttpError> {
    let secret = std::env::var(env_var).unwrap_or_default().trim().to_owned();
    if secret.is_empty() {
        return Err(HttpError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            format!("webhook receiver not configured (set ${env_var} and restart the server)"),
        ));
    }
    Ok(secret)
}

/// `_reject_signature` — one 403 for both "missing" and "wrong".
fn reject_signature() -> HttpError {
    HttpError::new(StatusCode::FORBIDDEN, "invalid or missing signature")
}

/// `_verify_hmac_sha256`.
fn verify_hmac_sha256(body: &[u8], signature_header: Option<&str>, secret: &str) -> bool {
    let Some(header) = signature_header else {
        return false;
    };
    if header.is_empty() || secret.is_empty() {
        return false;
    }
    let expected = hex_lower(&hmac_sha256(secret.as_bytes(), body));
    let received = header.trim();
    // Both the `sha256=` form GitHub sends and the bare hex some CI providers
    // send are accepted, which is what lets one helper cover both endpoints.
    let received = received.strip_prefix("sha256=").unwrap_or(received);
    if received.len() != expected.len() {
        return false;
    }
    compare_digest(received.as_bytes(), expected.as_bytes())
}

/// HMAC-SHA256 (RFC 2104) over the in-tree digest — see the module docs.
fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    use stax_etl::stats::sha256::digest;
    const BLOCK: usize = 64;
    let mut padded = [0_u8; BLOCK];
    if key.len() > BLOCK {
        padded[..32].copy_from_slice(&digest(key));
    } else {
        padded[..key.len()].copy_from_slice(key);
    }
    let mut inner = Vec::with_capacity(BLOCK + message.len());
    inner.extend(padded.iter().map(|byte| byte ^ 0x36));
    inner.extend_from_slice(message);
    let inner_digest = digest(&inner);
    let mut outer = Vec::with_capacity(BLOCK + 32);
    outer.extend(padded.iter().map(|byte| byte ^ 0x5c));
    outer.extend_from_slice(&inner_digest);
    digest(&outer)
}

fn hex_lower(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut out, byte| {
        let _ = write!(out, "{byte:02x}");
        out
    })
}

/// `hmac.compare_digest` for two equal-length byte strings.
fn compare_digest(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter()
        .zip(b.iter())
        .fold(0_u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

// ── handlers ─────────────────────────────────────────────────────────────────

async fn github_webhook(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    // Order is the reference's: secret, then body, then signature, then parse.
    // A 503 beats a bad signature, and both beat bad JSON.
    let secret = match require_secret(ENV_GITHUB_SECRET) {
        Ok(secret) => secret,
        Err(err) => return err.into_response(),
    };
    if !verify_hmac_sha256(&body, header_str(&headers, "x-hub-signature-256"), &secret) {
        return reject_signature().into_response();
    }
    let payload = match parse_payload(&body) {
        Ok(payload) => payload,
        Err(err) => return err.into_response(),
    };
    let event = header_str(&headers, "x-github-event").unwrap_or("unknown");
    match ingest_github_event(&state, event, &payload) {
        Ok(result) => plain_json(&result),
        Err(err) => err.into_response(),
    }
}

async fn gitlab_webhook(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let secret = match require_secret(ENV_GITLAB_SECRET) {
        Ok(secret) => secret,
        Err(err) => return err.into_response(),
    };
    // A static-token compare, still through `compare_digest`.
    let received = header_str(&headers, "x-gitlab-token")
        .unwrap_or_default()
        .trim()
        .to_owned();
    if received.is_empty() || !compare_digest(received.as_bytes(), secret.as_bytes()) {
        return reject_signature().into_response();
    }
    let payload = match parse_payload(&body) {
        Ok(payload) => payload,
        Err(err) => return err.into_response(),
    };
    // `X-Gitlab-Event` or the body's `object_kind` or `"unknown"`.
    let event = header_str(&headers, "x-gitlab-event")
        .map(str::to_owned)
        .or_else(|| truthy_str(payload.get("object_kind")))
        .unwrap_or_else(|| "unknown".to_owned());
    match ingest_gitlab_event(&state, &event, &payload) {
        Ok(result) => plain_json(&result),
        Err(err) => err.into_response(),
    }
}

async fn ci_webhook(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    let secret = match require_secret(ENV_CI_SECRET) {
        Ok(secret) => secret,
        Err(err) => return err.into_response(),
    };
    if !verify_hmac_sha256(
        &body,
        header_str(&headers, "x-webhook-signature-256"),
        &secret,
    ) {
        return reject_signature().into_response();
    }
    let payload = match parse_payload(&body) {
        Ok(payload) => payload,
        Err(err) => return err.into_response(),
    };
    let provider = truthy_str(payload.get("provider")).unwrap_or_else(|| "generic-ci".to_owned());
    let repo_slug = truthy_str(payload.get("repository"))
        .or_else(|| truthy_str(payload.get("repo_slug")))
        .unwrap_or_default();
    let row = normalise_ci_run_payload(
        &payload,
        &provider,
        // `repo_slug or None` — an empty string means "derive it yourself".
        if repo_slug.is_empty() {
            None
        } else {
            Some(repo_slug.as_str())
        },
    );
    let verb = match with_store(&state, |conn| upsert_ci_run(conn, &row)) {
        Ok(verb) => verb,
        Err(err) => return err.into_response(),
    };
    let mut result = Map::new();
    result.insert("status".to_owned(), Value::from("ok"));
    result.insert("verb".to_owned(), Value::from(verb));
    result.insert("run_id".to_owned(), row["run_id"].clone());
    plain_json(&result)
}

/// `_ingest_github_event`.
fn ingest_github_event(
    state: &AppState,
    event: &str,
    payload: &Map<String, Value>,
) -> Result<Map<String, Value>, HttpError> {
    if event == "ping" {
        let mut result = Map::new();
        result.insert("status".to_owned(), Value::from("pong"));
        return Ok(result);
    }

    if event == "pull_request" {
        let pr = object_or_empty(payload.get("pull_request"));
        let repo_full = truthy_str(object_or_empty(payload.get("repository")).get("full_name"));
        // `if not pr or not repo` — truthiness on the PR dict and on the
        // extracted `full_name`, in that order.
        if pr.is_empty() || repo_full.is_none() {
            return Err(HttpError::bad_request(
                "missing pull_request / repository fields",
            ));
        }
        let row = normalise_pr_payload(&pr, "github", repo_full.as_deref());
        let verb = with_store(state, |conn| upsert_pr_outcome(conn, &row))?;
        let mut result = Map::new();
        result.insert("status".to_owned(), Value::from("ok"));
        result.insert("kind".to_owned(), Value::from("pr"));
        result.insert("verb".to_owned(), Value::from(verb));
        result.insert("pr_number".to_owned(), row["pr_number"].clone());
        return Ok(result);
    }

    if event == "workflow_run" {
        let run = object_or_empty(payload.get("workflow_run"));
        if run.is_empty() {
            return Err(HttpError::bad_request("missing workflow_run field"));
        }
        // NOTE the asymmetry with `pull_request`: a missing repository is NOT a
        // 400 here, it is a `None` slug the normaliser then derives from the
        // run's own payload. Reproduced, asymmetry and all.
        let repo_full = truthy_str(object_or_empty(payload.get("repository")).get("full_name"));
        let row = normalise_ci_run_payload(&run, "github-actions", repo_full.as_deref());
        let verb = with_store(state, |conn| upsert_ci_run(conn, &row))?;
        let mut result = Map::new();
        result.insert("status".to_owned(), Value::from("ok"));
        result.insert("kind".to_owned(), Value::from("ci"));
        result.insert("verb".to_owned(), Value::from(verb));
        result.insert("run_id".to_owned(), row["run_id"].clone());
        return Ok(result);
    }

    let mut result = Map::new();
    result.insert("status".to_owned(), Value::from("ignored"));
    result.insert("event".to_owned(), Value::from(event));
    Ok(result)
}

/// `_ingest_gitlab_event`.
fn ingest_gitlab_event(
    state: &AppState,
    event: &str,
    payload: &Map<String, Value>,
) -> Result<Map<String, Value>, HttpError> {
    // `payload.get("object_kind") or event` — the body wins when truthy.
    let object_kind = truthy_str(payload.get("object_kind")).unwrap_or_else(|| event.to_owned());

    if object_kind == "merge_request" {
        let attrs = object_or_empty(payload.get("object_attributes"));
        let project = object_or_empty(payload.get("project"));
        let repo_slug = truthy_str(project.get("path_with_namespace"))
            .or_else(|| truthy_str(project.get("name_with_namespace")))
            .or_else(|| truthy_str(project.get("name")))
            .unwrap_or_default();
        let pr_number = truthy_int(attrs.get("iid"))
            .or_else(|| truthy_int(attrs.get("id")))
            .unwrap_or(0);
        let mut mr_state = truthy_str(attrs.get("state"))
            .unwrap_or_else(|| "open".to_owned())
            .to_lowercase();
        // `opened` and `locked` both become `open`; `closed` / `merged` pass
        // through as themselves.
        if mr_state == "opened" || mr_state == "locked" {
            mr_state = "open".to_owned();
        }
        let mut row = Map::new();
        row.insert("provider".to_owned(), Value::from("gitlab"));
        row.insert("repo_slug".to_owned(), Value::from(repo_slug));
        row.insert("pr_number".to_owned(), Value::from(pr_number));
        row.insert("title".to_owned(), str_or_null(attrs.get("title")));
        row.insert("state".to_owned(), Value::from(mr_state));
        row.insert("merged_at".to_owned(), str_or_null(attrs.get("merged_at")));
        row.insert("reverted_at".to_owned(), Value::Null);
        row.insert(
            "author".to_owned(),
            str_or_null(object_or_empty(payload.get("user")).get("username")),
        );
        row.insert(
            "raw_json".to_owned(),
            Value::from(dumps_py_default(&Value::Object(payload.clone()))),
        );
        let verb = with_store(state, |conn| upsert_pr_outcome(conn, &row))?;
        let mut result = Map::new();
        result.insert("status".to_owned(), Value::from("ok"));
        result.insert("kind".to_owned(), Value::from("pr"));
        result.insert("verb".to_owned(), Value::from(verb));
        result.insert("pr_number".to_owned(), Value::from(pr_number));
        return Ok(result);
    }

    if object_kind == "pipeline" {
        let attrs = object_or_empty(payload.get("object_attributes"));
        let project = object_or_empty(payload.get("project"));
        let repo_slug = truthy_str(project.get("path_with_namespace"))
            .or_else(|| truthy_str(project.get("name")))
            .unwrap_or_default();
        // `str(attrs.get("id") or 0)`.
        let run_id = truthy_int(attrs.get("id")).unwrap_or(0).to_string();
        let commit_sha = truthy_str(attrs.get("sha"))
            .or_else(|| truthy_str(object_or_empty(payload.get("commit")).get("id")))
            .unwrap_or_default();
        let status = gitlab_ci_status(truthy_str(attrs.get("status")).as_deref());
        let mut row = Map::new();
        row.insert("provider".to_owned(), Value::from("gitlab-ci"));
        row.insert("repo_slug".to_owned(), Value::from(repo_slug));
        row.insert("run_id".to_owned(), Value::from(run_id.clone()));
        row.insert("commit_sha".to_owned(), Value::from(commit_sha));
        row.insert("status".to_owned(), Value::from(status));
        row.insert(
            "workflow_name".to_owned(),
            truthy_str(attrs.get("ref")).map_or(Value::Null, Value::from),
        );
        row.insert(
            "started_ts".to_owned(),
            truthy_str(attrs.get("created_at")).map_or(Value::Null, Value::from),
        );
        row.insert(
            "completed_ts".to_owned(),
            truthy_str(attrs.get("finished_at")).map_or(Value::Null, Value::from),
        );
        row.insert(
            "raw_json".to_owned(),
            Value::from(dumps_py_default(&Value::Object(payload.clone()))),
        );
        let verb = with_store(state, |conn| upsert_ci_run(conn, &row))?;
        let mut result = Map::new();
        result.insert("status".to_owned(), Value::from("ok"));
        result.insert("kind".to_owned(), Value::from("ci"));
        result.insert("verb".to_owned(), Value::from(verb));
        result.insert("run_id".to_owned(), Value::from(run_id));
        return Ok(result);
    }

    let mut result = Map::new();
    result.insert("status".to_owned(), Value::from("ignored"));
    result.insert("object_kind".to_owned(), Value::from(object_kind));
    Ok(result)
}

/// GitLab's pipeline status enum → ours, `in_progress` as the catch-all.
///
/// Not the same table as [`normalise_ci_status`]: GitLab says `failed` where
/// GitHub says `failure`, and neither map is a superset of the other.
fn gitlab_ci_status(raw: Option<&str>) -> &'static str {
    match raw.unwrap_or_default().to_lowercase().as_str() {
        "success" => "success",
        "failed" => "failure",
        "canceled" | "cancelled" => "cancelled",
        "skipped" => "skipped",
        "running" => "in_progress",
        "manual" | "pending" | "preparing" | "scheduled" | "created" | "waiting_for_resource" => {
            "pending"
        }
        _ => "in_progress",
    }
}

// ── github_ingest normalisers ────────────────────────────────────────────────

/// `github_ingest.normalise_pr_payload`.
fn normalise_pr_payload(
    payload: &Map<String, Value>,
    provider: &str,
    repo_slug: Option<&str>,
) -> Map<String, Value> {
    let pr_number = truthy_int(payload.get("number"))
        .or_else(|| truthy_int(payload.get("id")))
        .unwrap_or(0);
    let author = str_or_null(object_or_empty(payload.get("user")).get("login"));

    let mut state = truthy_str(payload.get("state"))
        .unwrap_or_else(|| "open".to_owned())
        .to_lowercase();
    let merged = truthy_bool(payload.get("merged"));
    let merged_at = str_or_null(payload.get("merged_at"));
    // The one derived state, and it is derived AFTER the lowercase.
    if state == "closed" && (merged || !merged_at.is_null()) {
        state = "merged".to_owned();
    }

    let resolved_slug = repo_slug.map(str::to_owned).unwrap_or_else(|| {
        let repo = object_or_empty(object_or_empty(payload.get("base")).get("repo"));
        truthy_str(repo.get("full_name"))
            .or_else(|| truthy_str(repo.get("name")))
            .unwrap_or_default()
    });

    let mut row = Map::new();
    row.insert("provider".to_owned(), Value::from(provider));
    row.insert("repo_slug".to_owned(), Value::from(resolved_slug));
    row.insert("pr_number".to_owned(), Value::from(pr_number));
    row.insert("title".to_owned(), str_or_null(payload.get("title")));
    row.insert("state".to_owned(), Value::from(state));
    row.insert("merged_at".to_owned(), merged_at);
    // "downstream — Spec 22 fills this in".
    row.insert("reverted_at".to_owned(), Value::Null);
    row.insert("author".to_owned(), author);
    row.insert(
        "raw_json".to_owned(),
        Value::from(dumps_py_default(&Value::Object(payload.clone()))),
    );
    row
}

/// `github_ingest.normalise_ci_run_payload`.
fn normalise_ci_run_payload(
    payload: &Map<String, Value>,
    provider: &str,
    repo_slug: Option<&str>,
) -> Map<String, Value> {
    // `payload.get("id")` first, and only an ABSENT-or-None id falls through to
    // `run_id or 0` — a literal `0` id stays `"0"` rather than being retried.
    let run_id = match payload.get("id") {
        Some(Value::Null) | None => truthy_int(payload.get("run_id"))
            .map_or_else(|| "0".to_owned(), |value| value.to_string()),
        Some(value) => py_str(value),
    };

    let commit_sha = truthy_str(payload.get("head_sha"))
        .or_else(|| truthy_str(payload.get("sha")))
        .or_else(|| truthy_str(object_or_empty(payload.get("head_commit")).get("id")))
        .unwrap_or_default();

    let workflow_name = truthy_str(payload.get("name"))
        .or_else(|| truthy_str(payload.get("workflow_name")))
        .map_or(Value::Null, Value::from);

    let started_ts = truthy_str(payload.get("run_started_at"))
        .or_else(|| truthy_str(payload.get("created_at")))
        .map_or(Value::Null, Value::from);
    // `updated_at` is only carried when a `conclusion` is present.
    let completed_ts = if truthy_str(payload.get("conclusion")).is_some() {
        str_or_null(payload.get("updated_at"))
    } else {
        Value::Null
    };

    let status_raw =
        truthy_str(payload.get("conclusion")).or_else(|| truthy_str(payload.get("status")));
    let status = normalise_ci_status(status_raw.as_deref());

    let resolved_slug = repo_slug.map(str::to_owned).unwrap_or_else(|| {
        let repo = object_or_empty(payload.get("repository"));
        truthy_str(repo.get("full_name"))
            .or_else(|| truthy_str(repo.get("name")))
            .unwrap_or_default()
    });

    let mut row = Map::new();
    row.insert("provider".to_owned(), Value::from(provider));
    row.insert("repo_slug".to_owned(), Value::from(resolved_slug));
    row.insert("run_id".to_owned(), Value::from(run_id));
    row.insert("commit_sha".to_owned(), Value::from(commit_sha));
    row.insert("status".to_owned(), Value::from(status));
    row.insert("workflow_name".to_owned(), workflow_name);
    row.insert("started_ts".to_owned(), started_ts);
    row.insert("completed_ts".to_owned(), completed_ts);
    row.insert(
        "raw_json".to_owned(),
        Value::from(dumps_py_default(&Value::Object(payload.clone()))),
    );
    row
}

/// `_normalise_ci_status` — conservative; unknown means `in_progress`, so the
/// row is still inserted and `raw_json` keeps the original.
fn normalise_ci_status(raw: Option<&str>) -> &'static str {
    let Some(raw) = raw else {
        return "in_progress";
    };
    match raw.to_lowercase().as_str() {
        "success" | "successful" => "success",
        "failure" | "failed" | "timed_out" => "failure",
        "cancelled" | "canceled" => "cancelled",
        "skipped" | "neutral" => "skipped",
        "queued" | "waiting" | "pending" | "requested" | "action_required" => "pending",
        _ => "in_progress",
    }
}

// ── upserts ──────────────────────────────────────────────────────────────────

/// `github_ingest.upsert_pr_outcome` — SELECT, then INSERT or UPDATE.
///
/// Not an `INSERT … ON CONFLICT`: the reference reads first so it can report
/// `"inserted"` / `"updated"`, and the UPDATE keeps `reverted_at` behind a
/// `COALESCE(?, reverted_at)` so a webhook can never clear a revert recorded
/// downstream.
fn upsert_pr_outcome(
    conn: &Connection,
    row: &Map<String, Value>,
) -> rusqlite::Result<&'static str> {
    let provider = text(row, "provider");
    let repo_slug = text(row, "repo_slug");
    let pr_number = row["pr_number"].as_i64().unwrap_or(0);
    let existing: Option<i64> = conn
        .query_row(
            "SELECT id FROM pr_outcomes WHERE provider=? AND repo_slug=? AND pr_number=?",
            rusqlite::params![provider, repo_slug, pr_number],
            |r| r.get(0),
        )
        .ok();
    if existing.is_none() {
        conn.execute(
            "INSERT INTO pr_outcomes \
             (provider, repo_slug, pr_number, title, state, merged_at, \
              reverted_at, author, raw_json) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
            rusqlite::params![
                provider,
                repo_slug,
                pr_number,
                opt_text(row, "title"),
                text(row, "state"),
                opt_text(row, "merged_at"),
                opt_text(row, "reverted_at"),
                opt_text(row, "author"),
                text(row, "raw_json"),
            ],
        )?;
        return Ok("inserted");
    }
    conn.execute(
        "UPDATE pr_outcomes SET title=?, state=?, merged_at=?, \
          reverted_at=COALESCE(?, reverted_at), author=?, raw_json=? \
         WHERE provider=? AND repo_slug=? AND pr_number=?",
        rusqlite::params![
            opt_text(row, "title"),
            text(row, "state"),
            opt_text(row, "merged_at"),
            opt_text(row, "reverted_at"),
            opt_text(row, "author"),
            text(row, "raw_json"),
            provider,
            repo_slug,
            pr_number,
        ],
    )?;
    Ok("updated")
}

/// `github_ingest.upsert_ci_run`.
fn upsert_ci_run(conn: &Connection, row: &Map<String, Value>) -> rusqlite::Result<&'static str> {
    let provider = text(row, "provider");
    let run_id = text(row, "run_id");
    let existing: Option<i64> = conn
        .query_row(
            "SELECT id FROM ci_runs WHERE provider=? AND run_id=?",
            rusqlite::params![provider, run_id],
            |r| r.get(0),
        )
        .ok();
    if existing.is_none() {
        conn.execute(
            "INSERT INTO ci_runs \
             (provider, repo_slug, run_id, commit_sha, status, \
              workflow_name, started_ts, completed_ts, raw_json) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
            rusqlite::params![
                provider,
                text(row, "repo_slug"),
                run_id,
                text(row, "commit_sha"),
                text(row, "status"),
                opt_text(row, "workflow_name"),
                opt_text(row, "started_ts"),
                opt_text(row, "completed_ts"),
                text(row, "raw_json"),
            ],
        )?;
        return Ok("inserted");
    }
    conn.execute(
        "UPDATE ci_runs SET repo_slug=?, commit_sha=?, status=?, \
          workflow_name=?, started_ts=?, completed_ts=?, raw_json=? \
         WHERE provider=? AND run_id=?",
        rusqlite::params![
            text(row, "repo_slug"),
            text(row, "commit_sha"),
            text(row, "status"),
            opt_text(row, "workflow_name"),
            opt_text(row, "started_ts"),
            opt_text(row, "completed_ts"),
            text(row, "raw_json"),
            provider,
            run_id,
        ],
    )?;
    Ok("updated")
}

/// `_open_store()` — `db.connect` + `schema.apply`, then close.
///
/// The migration is not ported (DIV-134): the server that owns this store has
/// already applied it, and a webhook receiver must not be the thing that
/// migrates a database.
fn with_store<T>(
    state: &AppState,
    body: impl FnOnce(&Connection) -> rusqlite::Result<T>,
) -> Result<T, HttpError> {
    let conn = state
        .connect()
        .map_err(|err| HttpError::new(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?;
    body(&conn).map_err(|err| HttpError::new(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))
}

// ── plumbing ─────────────────────────────────────────────────────────────────

/// `json.loads(body or b"{}")` plus the `isinstance(payload, dict)` guard.
fn parse_payload(body: &Bytes) -> Result<Map<String, Value>, HttpError> {
    let text = if body.is_empty() {
        "{}"
    } else {
        // Invalid UTF-8 raises `UnicodeDecodeError` inside `json.loads`, which
        // the `except json.JSONDecodeError` does NOT catch — Python 500s. A
        // byte no parser accepts reproduces the 400 side of it; the difference
        // is recorded in DIV-137 alongside the message caveat.
        std::str::from_utf8(body).unwrap_or("\u{0}")
    };
    match serde_json::from_str::<Value>(text) {
        Ok(Value::Object(map)) => Ok(map),
        Ok(_) => Err(HttpError::bad_request("payload must be an object")),
        // `f"invalid JSON: {exc}"` embeds CPython's decoder message, which no
        // other parser reproduces. The prefix is exact; the text after it is
        // this parser's — DIV-137, recorded rather than faked.
        Err(err) => Err(HttpError::bad_request(format!("invalid JSON: {err}"))),
    }
}

/// `Response(content=json.dumps(result), media_type="application/json")`.
fn plain_json(result: &Map<String, Value>) -> Response {
    (
        StatusCode::OK,
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static(JSON_CONTENT_TYPE),
        )],
        dumps_py_default(&Value::Object(result.clone())),
    )
        .into_response()
}

/// `json.dumps(obj)` with **every default** — `ensure_ascii=True` and the
/// `(", ", ": ")` separators. See the module docs for why this is a third
/// writer rather than one of the two the crate already has.
///
/// Scalars delegate to `pyjson::dumps_compact`, which owns the escaping and
/// CPython's float `repr`; only the container separators are written here, which
/// is exactly the difference between the two layouts.
fn dumps_py_default(value: &Value) -> String {
    match value {
        Value::Object(map) => {
            let body = map
                .iter()
                .map(|(key, val)| {
                    format!(
                        "{}: {}",
                        stax_memory::pyjson::dumps_compact(&Value::String(key.clone())),
                        dumps_py_default(val)
                    )
                })
                .collect::<Vec<_>>()
                .join(", ");
            format!("{{{body}}}")
        }
        Value::Array(items) => {
            let body = items
                .iter()
                .map(dumps_py_default)
                .collect::<Vec<_>>()
                .join(", ");
            format!("[{body}]")
        }
        scalar => stax_memory::pyjson::dumps_compact(scalar),
    }
}

fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|value| value.to_str().ok())
}

/// `payload.get("x") or {}` — missing, null, and an empty dict all land on `{}`.
fn object_or_empty(value: Option<&Value>) -> Map<String, Value> {
    value
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default()
}

/// `x or y` over a string-ish value: falsy is absent, null, or `""`.
fn truthy_str(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(text) if !text.is_empty() => Some(text.clone()),
        Value::Null | Value::String(_) => None,
        other => {
            let rendered = py_str(other);
            (!rendered.is_empty() && rendered != "0" && rendered != "False").then_some(rendered)
        }
    }
}

fn truthy_int(value: Option<&Value>) -> Option<i64> {
    match value? {
        Value::Number(number) => {
            let value = number.as_i64().or_else(|| {
                #[allow(clippy::cast_possible_truncation)]
                number.as_f64().map(|f| f as i64)
            })?;
            (value != 0).then_some(value)
        }
        // `int("12")` — `int()` accepts a numeric string.
        Value::String(text) if !text.is_empty() => {
            text.trim().parse::<i64>().ok().filter(|value| *value != 0)
        }
        _ => None,
    }
}

fn truthy_bool(value: Option<&Value>) -> bool {
    match value {
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
        _ => false,
    }
}

/// `str(x) if x is not None else None`.
fn str_or_null(value: Option<&Value>) -> Value {
    match value {
        None | Some(Value::Null) => Value::Null,
        Some(other) => Value::from(py_str(other)),
    }
}

/// CPython's `str()` of a JSON scalar — `True`, not `true`.
fn py_str(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Bool(true) => "True".to_owned(),
        Value::Bool(false) => "False".to_owned(),
        Value::Null => "None".to_owned(),
        Value::Number(number) => number.to_string(),
        other => other.to_string(),
    }
}

fn text(row: &Map<String, Value>, key: &str) -> String {
    row.get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

fn opt_text(row: &Map<String, Value>, key: &str) -> Option<String> {
    match row.get(key) {
        Some(Value::String(text)) => Some(text.clone()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> AppState {
        AppState::new(
            std::path::PathBuf::from("/nonexistent/store.db"),
            std::path::PathBuf::from("/nonexistent"),
            crate::state::Config::default(),
        )
    }

    #[test]
    fn an_unconfigured_receiver_is_a_503_naming_the_env_var() {
        let err = require_secret("STACKUNDERFLOW_DEFINITELY_UNSET_SECRET").expect_err("503");
        assert_eq!(
            err.body().render(),
            r#"{"detail":"webhook receiver not configured (set $STACKUNDERFLOW_DEFINITELY_UNSET_SECRET and restart the server)"}"#
        );
    }

    #[test]
    fn the_hmac_matches_rfc_4231() {
        // The standard vectors, so the in-tree SHA-256 is pinned against
        // something outside this repository.
        assert_eq!(
            hex_lower(&hmac_sha256(b"Jefe", b"what do ya want for nothing?")),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        assert_eq!(
            hex_lower(&hmac_sha256(&[0x0b; 20], b"Hi There")),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
        // Test case 6: a 131-byte key, which exercises the >block-size fold.
        assert_eq!(
            hex_lower(&hmac_sha256(
                &[0xaa; 131],
                b"Test Using Larger Than Block-Size Key - Hash Key First"
            )),
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );
    }

    #[test]
    fn the_verifier_takes_both_header_forms_and_rejects_by_length_first() {
        let digest = hex_lower(&hmac_sha256(b"secret", b"payload"));
        assert!(verify_hmac_sha256(
            b"payload",
            Some(&format!("sha256={digest}")),
            "secret"
        ));
        assert!(verify_hmac_sha256(b"payload", Some(&digest), "secret"));
        assert!(!verify_hmac_sha256(b"payload", Some(&digest), "wrong"));
        assert!(!verify_hmac_sha256(b"other", Some(&digest), "secret"));
        assert!(!verify_hmac_sha256(b"payload", None, "secret"));
        assert!(!verify_hmac_sha256(b"payload", Some(""), "secret"));
        // Truncated: rejected on LENGTH, before the constant-time compare.
        assert!(!verify_hmac_sha256(
            b"payload",
            Some(&digest[..10]),
            "secret"
        ));
    }

    #[test]
    fn the_body_writer_is_neither_the_cli_nor_the_http_one() {
        let mut result = Map::new();
        result.insert("status".to_owned(), Value::from("pong"));
        assert_eq!(
            dumps_py_default(&Value::Object(result)),
            r#"{"status": "pong"}"#
        );
        // `ensure_ascii=True`, unlike the HTTP writer: the é goes out as the
        // six ASCII bytes `é`, where `JSONResponse` would ship the two
        // raw UTF-8 ones. Same value, different bytes, and bytes are the
        // contract (finding 11).
        assert_eq!(
            dumps_py_default(&serde_json::json!({"event": "café"})),
            "{\"event\": \"caf\\u00e9\"}"
        );
        assert_eq!(
            stax_memory::pyjson::dumps_http(&serde_json::json!({"event": "café"})),
            "{\"event\":\"café\"}"
        );
        // The separators hold all the way down, and empties stay collapsed.
        assert_eq!(
            dumps_py_default(&serde_json::json!({"a": [1, {"b": 2}]})),
            r#"{"a": [1, {"b": 2}]}"#
        );
        assert_eq!(dumps_py_default(&serde_json::json!({})), "{}");
        assert_eq!(dumps_py_default(&serde_json::json!([])), "[]");
    }

    #[test]
    fn a_ping_never_touches_the_store_and_an_unknown_event_is_ignored() {
        let state = state();
        let empty = Map::new();
        // The store path does not exist; that these three legs answer at all is
        // the proof that none of them opens it.
        let pong = ingest_github_event(&state, "ping", &empty).expect("pong");
        assert_eq!(
            dumps_py_default(&Value::Object(pong)),
            r#"{"status": "pong"}"#
        );
        let ignored = ingest_github_event(&state, "issues", &empty).expect("ignored");
        assert_eq!(
            dumps_py_default(&Value::Object(ignored)),
            r#"{"status": "ignored", "event": "issues"}"#
        );
        let ignored = ingest_gitlab_event(&state, "note", &empty).expect("ignored");
        assert_eq!(
            dumps_py_default(&Value::Object(ignored)),
            r#"{"status": "ignored", "object_kind": "note"}"#
        );
    }

    #[test]
    fn a_missing_pr_or_repo_is_a_400_before_the_store_opens() {
        let state = state();
        let payload = serde_json::json!({"pull_request": {"number": 1}});
        let err = ingest_github_event(&state, "pull_request", payload.as_object().expect("obj"))
            .expect_err("400");
        assert_eq!(
            err.body().render(),
            r#"{"detail":"missing pull_request / repository fields"}"#
        );
        let err = ingest_github_event(&state, "workflow_run", &Map::new()).expect_err("400");
        assert_eq!(
            err.body().render(),
            r#"{"detail":"missing workflow_run field"}"#
        );
    }

    #[test]
    fn a_closed_and_merged_pr_becomes_the_merged_state() {
        let payload = serde_json::json!({
            "number": 7, "title": "t", "state": "CLOSED", "merged": true,
            "user": {"login": "yad"}
        });
        let row = normalise_pr_payload(payload.as_object().expect("obj"), "github", Some("o/r"));
        assert_eq!(row["state"], Value::from("merged"));
        assert_eq!(row["pr_number"], Value::from(7));
        assert_eq!(row["author"], Value::from("yad"));
        assert_eq!(row["reverted_at"], Value::Null);

        // Closed with neither flag stays closed.
        let payload = serde_json::json!({"number": 7, "state": "closed"});
        let row = normalise_pr_payload(payload.as_object().expect("obj"), "github", Some("o/r"));
        assert_eq!(row["state"], Value::from("closed"));

        // …and `merged_at` alone is enough, without the `merged` flag.
        let payload =
            serde_json::json!({"number": 7, "state": "closed", "merged_at": "2026-01-01"});
        let row = normalise_pr_payload(payload.as_object().expect("obj"), "github", Some("o/r"));
        assert_eq!(row["state"], Value::from("merged"));
    }

    #[test]
    fn the_repo_slug_falls_back_to_the_prs_own_base() {
        let payload = serde_json::json!({
            "number": 1, "base": {"repo": {"full_name": "o/from-base"}}
        });
        let row = normalise_pr_payload(payload.as_object().expect("obj"), "github", None);
        assert_eq!(row["repo_slug"], Value::from("o/from-base"));
    }

    #[test]
    fn completed_ts_is_only_carried_when_a_conclusion_is() {
        let with_conclusion = serde_json::json!({
            "id": 42, "conclusion": "failure", "updated_at": "2026-01-02T03:04:05Z",
            "head_sha": "abc"
        });
        let row = normalise_ci_run_payload(
            with_conclusion.as_object().expect("obj"),
            "github-actions",
            Some("o/r"),
        );
        assert_eq!(row["completed_ts"], Value::from("2026-01-02T03:04:05Z"));
        assert_eq!(row["status"], Value::from("failure"));
        // `str(42)` is `"42"`, not `"42.0"`.
        assert_eq!(row["run_id"], Value::from("42"));

        let in_flight = serde_json::json!({
            "id": 42, "status": "queued", "updated_at": "2026-01-02T03:04:05Z"
        });
        let row = normalise_ci_run_payload(
            in_flight.as_object().expect("obj"),
            "github-actions",
            Some("o/r"),
        );
        assert_eq!(row["completed_ts"], Value::Null);
        assert_eq!(row["status"], Value::from("pending"));
    }

    #[test]
    fn an_unknown_ci_status_is_in_progress_and_the_two_tables_disagree() {
        assert_eq!(normalise_ci_status(None), "in_progress");
        assert_eq!(normalise_ci_status(Some("MADE_UP")), "in_progress");
        assert_eq!(normalise_ci_status(Some("timed_out")), "failure");
        assert_eq!(normalise_ci_status(Some("neutral")), "skipped");
        assert_eq!(normalise_ci_status(Some("action_required")), "pending");
        // GitLab says `failed`; GitHub says `failure`. Neither map covers the
        // other, which is why there are two.
        assert_eq!(gitlab_ci_status(Some("failed")), "failure");
        assert_eq!(normalise_ci_status(Some("failed")), "failure");
        assert_eq!(gitlab_ci_status(Some("manual")), "pending");
        assert_eq!(gitlab_ci_status(Some("neutral")), "in_progress");
        assert_eq!(gitlab_ci_status(None), "in_progress");
    }

    #[test]
    fn a_non_object_body_is_a_400_and_an_empty_body_is_an_empty_object() {
        assert_eq!(
            parse_payload(&Bytes::from_static(b"")).expect("empty"),
            Map::new()
        );
        let err = parse_payload(&Bytes::from_static(b"[1,2]")).expect_err("400");
        assert_eq!(
            err.body().render(),
            r#"{"detail":"payload must be an object"}"#
        );
        let err = parse_payload(&Bytes::from_static(b"{oops")).expect_err("400");
        assert!(err.body().render().contains("invalid JSON: "));
    }
}
