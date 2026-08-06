//! `services/github_ingest.py`'s normalisers + upserts — shared by the
//! webhook receiver (`stax-server`) and the REST backfill (`stax-cli`).
//!
//! Moved here from `stax-server/src/routes/webhooks.rs` when DIV-199 landed
//! (2026-08-06): the backfill needs these exact functions and `stax-cli` may
//! not link `stax-server` (DIV-279/308). `stax-server` imports them back, so
//! the receiver and the backfill share one implementation exactly as the
//! reference intends. The py-semantics helpers (`truthy_*`, `py_str`,
//! `str_or_null`) travel with them because they ARE the normalisation.

use rusqlite::Connection;
use serde_json::{Map, Value};

// ── github_ingest normalisers ────────────────────────────────────────────────

/// `github_ingest.normalise_pr_payload`.
pub fn normalise_pr_payload(
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
pub fn normalise_ci_run_payload(
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
pub fn normalise_ci_status(raw: Option<&str>) -> &'static str {
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
pub fn upsert_pr_outcome(
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
pub fn upsert_ci_run(
    conn: &Connection,
    row: &Map<String, Value>,
) -> rusqlite::Result<&'static str> {
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

/// `x or y` over a string-ish value: falsy is absent, null, or `""`.
pub fn truthy_str(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(text) if !text.is_empty() => Some(text.clone()),
        Value::Null | Value::String(_) => None,
        other => {
            let rendered = py_str(other);
            (!rendered.is_empty() && rendered != "0" && rendered != "False").then_some(rendered)
        }
    }
}

pub fn truthy_int(value: Option<&Value>) -> Option<i64> {
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

pub fn truthy_bool(value: Option<&Value>) -> bool {
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
pub fn str_or_null(value: Option<&Value>) -> Value {
    match value {
        None | Some(Value::Null) => Value::Null,
        Some(other) => Value::from(py_str(other)),
    }
}

/// CPython's `str()` of a JSON scalar — `True`, not `true`.
pub fn py_str(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Bool(true) => "True".to_owned(),
        Value::Bool(false) => "False".to_owned(),
        Value::Null => "None".to_owned(),
        Value::Number(number) => number.to_string(),
        other => other.to_string(),
    }
}

pub fn text(row: &Map<String, Value>, key: &str) -> String {
    row.get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

pub fn opt_text(row: &Map<String, Value>, key: &str) -> Option<String> {
    match row.get(key) {
        Some(Value::String(text)) => Some(text.clone()),
        _ => None,
    }
}

/// `payload.get("x") or {}` — missing, null, and an empty dict all land on `{}`.
pub fn object_or_empty(value: Option<&Value>) -> Map<String, Value> {
    value
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default()
}

/// `json.dumps(obj)` with **every default** — `ensure_ascii=True` and the
/// `(", ", ": ")` separators. See the module docs for why this is a third
/// writer rather than one of the two the crate already has.
///
/// Batch D wrote this file-locally because `stax-memory` is another crate; the
/// wave-5 dedup pass moved it into `pyjson` as a fourth `Layout`, next to the
/// two writers it has to stay distinguishable from. This alias keeps the call
/// sites and the module docs reading the way they were measured.
pub fn dumps_py_default(value: &Value) -> String {
    stax_memory::pyjson::dumps_py_default(value)
}
