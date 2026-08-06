//! Per-session static analysis — port of
//! `stackunderflow/services/static_analysis/` (Spec 21, issue #93), the last
//! parked node family of the port campaign (2026-08-06).
//!
//! Pre/post static-analysis deltas for every file a session touched:
//! cyclomatic complexity, lint count, type completeness. Sessions are
//! reconstructed via Playback v2 ([`crate::services::playback_fs`]) into a
//! tmpdir the runner deletes; parsed metric values persist to
//! `static_analysis_findings` (schema v018), never raw source.
//!
//! Analyzer contract, as the reference states it: `analyze(path, content) ->
//! FileMetrics` and `available() -> (bool, reason)`. A missing tool is a
//! warning and a skipped metric — never a hard failure.
//!
//! # Recorded divergence — the Python analyzer's in-process imports
//!
//! The reference imports `radon` as a *library* and drives `mypy` via its API
//! where available. A Rust process cannot; both go through their CLI entry
//! points here, parsing the same documented output formats. Absent tools
//! behave identically (skip + reason). Six external tools are not present on
//! the campaign machine, so verification is structural (parsers pinned by
//! canned-output fixtures) rather than oracle-diffed — recorded honestly, per
//! the maintainer's translate-first order.

pub mod go_analyzer;
pub mod python_analyzer;
pub mod runner;
pub mod typescript_analyzer;

use serde_json::{Map, Value};

/// `python_analyzer.FileMetrics` — one analyzer pass over one file.
///
/// A metric *absent* from `metrics` means the analyzer couldn't produce a
/// value (tool missing, file empty, parse failure); the reason lives in
/// `warnings`. `details` carries per-metric extras that persist as the row's
/// `details_json`.
#[derive(Debug, Clone, Default)]
pub struct FileMetrics {
    /// Metric name → value. The reference's `dict[str, float]`; values stay
    /// `Value` so `details_json` round-trips CPython's float rendering.
    pub metrics: Map<String, Value>,
    /// Per-metric extras (lint rule frequency, avg vs max complexity).
    pub details: Map<String, Value>,
    /// Why a metric is missing, one line per reason.
    pub warnings: Vec<String>,
}

// ── the bin entry — `stax-server --analyze <json>` (DIV-308 spawn shape) ─────

/// Run one analyze verb from the bin's `--analyze` request JSON.
///
/// Returns `(stdout_payload, exit_code)`. Success is the verb's dict; failure
/// is `{"error": {"kind", "message"}}` with exit 1 — `kind: "bad_parameter"`
/// maps onto click's `BadParameter` rendering in `stax-cli`.
#[must_use]
pub fn run_bin_request(request: &str, store_path: &std::path::Path) -> (String, i32) {
    let fail = |kind: &str, message: String| -> (String, i32) {
        let payload = serde_json::json!({"error": {"kind": kind, "message": message}});
        (stax_memory::pyjson::dumps_compact(&payload), 1)
    };

    let Ok(request) = serde_json::from_str::<Value>(request) else {
        return fail("bad_request", "unparseable --analyze request".to_owned());
    };
    let verb = request
        .get("verb")
        .and_then(Value::as_str)
        .unwrap_or_default();

    // `_open_store()` — read-write + `schema.apply`, as every CLI verb does.
    let conn = match stax_etl::ingest::guard::open_read_write(store_path) {
        Ok(conn) => conn,
        Err(err) => return fail("store", err.to_string()),
    };
    if let Err(err) = stax_core::schema::apply(&conn) {
        return fail("store", err.to_string());
    }

    match verb {
        "session" => {
            let session_id = request
                .get("session_id")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let only: Option<Vec<String>> =
                request
                    .get("languages")
                    .and_then(Value::as_array)
                    .map(|list| {
                        list.iter()
                            .filter_map(Value::as_str)
                            .map(str::to_owned)
                            .collect()
                    });
            let only = only.filter(|list| !list.is_empty());
            match runner::analyze_session(&conn, session_id, only.as_deref()) {
                Ok(outcome) => (
                    stax_memory::pyjson::dumps_compact(&Value::Object(outcome.to_dict())),
                    0,
                ),
                // The reference maps `ValueError` onto `click.BadParameter`.
                Err(message) => fail("bad_parameter", message),
            }
        }
        "backfill" => {
            let since = request
                .get("since")
                .and_then(Value::as_str)
                .map(str::to_owned);
            let limit = request.get("limit").and_then(Value::as_i64);
            let concurrency = request
                .get("concurrency")
                .and_then(Value::as_u64)
                .map(|value| usize::try_from(value).unwrap_or(1));
            let store = store_path.to_path_buf();
            let factory = move || -> Option<rusqlite::Connection> {
                let conn = stax_etl::ingest::guard::open_read_write(&store).ok()?;
                stax_core::schema::apply(&conn).ok()?;
                Some(conn)
            };
            let report =
                runner::backfill(&conn, since.as_deref(), limit, concurrency, Some(&factory));
            (
                stax_memory::pyjson::dumps_compact(&Value::Object(report)),
                0,
            )
        }
        other => fail("bad_request", format!("unknown analyze verb: {other}")),
    }
}
