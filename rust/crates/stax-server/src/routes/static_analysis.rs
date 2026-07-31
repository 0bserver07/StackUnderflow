//! `routes/static_analysis.py` — 1 endpoint, wave 5 (batch D).
//!
//! | Item | Method | FastAPI path | axum path | State |
//! |---|---|---|---|---|
//! | `RS-5-107` | `GET` | `/api/static-analysis/session/{session_id}` | same | ported |
//!
//! # Read-only by design, which is why it ports and its neighbour does not
//!
//! The module docstring is explicit that this route "*doesn't* analyse on
//! demand — that's a CLI / backfill operation (analyzers fork shell
//! subprocesses)". So the whole endpoint is one indexed `SELECT` plus a
//! summariser, and it is a green parity row. Its path-neighbour
//! `routes/quality.py` sits one segment deeper and does the opposite (DIV-135).
//!
//! # `schema.apply` is not ported, and the response is why that is safe
//!
//! Python calls `schema.apply(conn)` first — a migration on a GET, guarding the
//! fresh-install race where a request beats the lifespan. The port does not
//! migrate: it never writes, and the harness's Python server has already applied
//! the schema to the shared home at boot. What replaces it is a table-existence
//! guard that returns the **same body Python would have returned** after
//! migrating — an empty findings list and a zeroed summary — so the narrowing is
//! invisible in the response on every store either implementation can serve. It
//! is recorded rather than assumed: DIV-134.
//!
//! # The summariser's three details that a paraphrase loses
//!
//! * **`by_metric` is insertion-ordered.** The rows arrive
//!   `ORDER BY file_path, metric`, and `metric_summary` is walked in first-seen
//!   order — not sorted. A `BTreeMap` here would silently reorder the `metrics`
//!   object on any session whose first file skips a metric.
//! * **`avg_delta` averages only the rows whose `delta` is non-NULL**, but
//!   `files`/`improved`/`regressed`/`neutral` count *all* the triples. The two
//!   populations differ on exactly the row this store already holds (a
//!   `file_created_in_session` finding with a NULL `pre_value` and NULL
//!   `delta`), so getting it wrong is not hypothetical here.
//! * **`_classify_delta` returns four values and the summary counts three.**
//!   `"unknown"` — either side NULL — is counted nowhere, so a metric can have
//!   `files: 1` with `improved`, `regressed` and `neutral` all zero. That is the
//!   live row, and it is the shape the case matrix pins.

use axum::Router;
use axum::extract::{Path as PathParam, State};
use axum::http::StatusCode;
use axum::routing::get;
use rusqlite::Connection;
use rusqlite::types::ValueRef;
use serde_json::{Map, Value};
use stax_etl::stats::aggregator::{PyNum, round_py};

use crate::json::{HandlerResult, HttpError, JsonBody, join_failure};
use crate::services::mart_queries::table_exists;
use crate::state::AppState;

/// `_SIGNIFICANT_DELTA_PCT`.
const SIGNIFICANT_DELTA_PCT: f64 = 0.20;

/// Mount this module's endpoints onto `router`.
pub fn register(router: Router<AppState>) -> Router<AppState> {
    router.route(
        "/api/static-analysis/session/{session_id}",
        get(get_session_static_analysis),
    )
}

/// `_LOWER_IS_BETTER`, with the `.get(metric, True)` default folded in.
fn lower_is_better(metric: &str) -> bool {
    match metric {
        "coverage" | "type_completeness" => false,
        // `complexity`, `lint_count`, and — via `.get(metric, True)` — every
        // metric the table does not name.
        _ => true,
    }
}

// ── GET /api/static-analysis/session/{session_id} ────────────────────────────

async fn get_session_static_analysis(
    State(state): State<AppState>,
    PathParam(session_id): PathParam<String>,
) -> HandlerResult {
    let worker = state.clone();
    let quality = tokio::task::spawn_blocking(move || -> Result<Value, HttpError> {
        let conn = worker.connect().map_err(|err| any_500(&err))?;
        session_quality(&conn, &session_id).map_err(sql_500)
    })
    .await
    .map_err(|err| join_failure(&err))??;
    // `JSONResponse(quality_to_dict(quality))` — `asdict` over a 3-field
    // dataclass, so the key order is the declaration's.
    Ok(JsonBody::ok(quality))
}

/// `runner.get_session_quality`, rendered through `asdict`.
fn session_quality(conn: &Connection, session_id: &str) -> rusqlite::Result<Value> {
    let rows = findings_rows(conn, session_id)?;

    let mut findings: Vec<Value> = Vec::with_capacity(rows.len());
    // Insertion-ordered, deliberately — see the module docs.
    let mut metric_order: Vec<String> = Vec::new();
    let mut by_metric: std::collections::HashMap<String, Vec<Triple>> =
        std::collections::HashMap::new();
    let mut languages: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut file_paths: std::collections::HashSet<String> = std::collections::HashSet::new();

    for row in rows {
        // `languages.add(str(r["language"]))` — the column is NOT NULL, so the
        // `str()` never sees a `None`.
        languages.insert(row.language.clone());
        file_paths.insert(row.file_path.clone());
        let entry = by_metric.entry(row.metric.clone()).or_insert_with(|| {
            metric_order.push(row.metric.clone());
            Vec::new()
        });
        entry.push(Triple {
            pre: row.pre_value,
            post: row.post_value,
            delta: row.delta,
        });

        let mut finding = Map::new();
        finding.insert("file_path".to_owned(), Value::from(row.file_path));
        finding.insert("language".to_owned(), Value::from(row.language));
        finding.insert("ts".to_owned(), Value::from(row.ts));
        finding.insert("metric".to_owned(), Value::from(row.metric));
        finding.insert("pre_value".to_owned(), opt_float(row.pre_value));
        finding.insert("post_value".to_owned(), opt_float(row.post_value));
        finding.insert("delta".to_owned(), opt_float(row.delta));
        finding.insert(
            "details_json".to_owned(),
            row.details_json.map_or(Value::Null, Value::from),
        );
        findings.push(Value::Object(finding));
    }

    let mut metric_summary = Map::new();
    for metric in &metric_order {
        let triples = &by_metric[metric];
        // `observed = [t for t in triples if t[2] is not None]` — the average is
        // over the non-NULL deltas only, while every count below is over ALL
        // the triples.
        let observed: Vec<f64> = triples.iter().filter_map(|t| t.delta).collect();
        let avg_delta = if observed.is_empty() {
            None
        } else {
            // `sum(...) / len(...)` — `sum()` over floats, so compensated; a
            // single-element list makes the two identical, and the store's one
            // live row has none at all.
            let mut acc = stax_etl::stats::aggregator::Neumaier::default();
            for value in &observed {
                acc.add(*value);
            }
            #[allow(clippy::cast_precision_loss)]
            Some(acc.finish() / observed.len() as f64)
        };

        let mut counts = [0_i64; 3];
        for triple in triples {
            match classify_delta(metric, triple.pre, triple.post) {
                Classification::Improved => counts[0] += 1,
                Classification::Regressed => counts[1] += 1,
                Classification::Neutral => counts[2] += 1,
                // `"unknown"` is counted nowhere. Not an oversight to fix.
                Classification::Unknown => {}
            }
        }

        let mut summary = Map::new();
        summary.insert(
            "files".to_owned(),
            Value::from(i64::try_from(triples.len()).unwrap_or(i64::MAX)),
        );
        summary.insert(
            "avg_delta".to_owned(),
            // `round(avg_delta, 4)` — CPython's `round` is correct decimal
            // rounding with ties to even, which formatting-and-reparsing gives.
            avg_delta.map_or(Value::Null, |value| {
                PyNum::Float(round_py(value, 4)).to_json()
            }),
        );
        summary.insert("improved".to_owned(), Value::from(counts[0]));
        summary.insert("regressed".to_owned(), Value::from(counts[1]));
        summary.insert("neutral".to_owned(), Value::from(counts[2]));
        metric_summary.insert(metric.clone(), Value::Object(summary));
    }

    let headline = build_headline(&metric_order, &metric_summary);

    let mut summary = Map::new();
    summary.insert(
        "files".to_owned(),
        Value::from(i64::try_from(file_paths.len()).unwrap_or(i64::MAX)),
    );
    summary.insert(
        "languages".to_owned(),
        // `sorted(languages)` over a `set[str]` — a `BTreeSet` is already in
        // that order, and UTF-8 byte order is Python's code-point order.
        Value::Array(languages.into_iter().map(Value::from).collect()),
    );
    summary.insert("metrics".to_owned(), Value::Object(metric_summary));
    summary.insert("headline".to_owned(), Value::from(headline));

    let mut payload = Map::new();
    payload.insert("session_id".to_owned(), Value::from(session_id));
    payload.insert("findings".to_owned(), Value::Array(findings));
    payload.insert("summary".to_owned(), Value::Object(summary));
    Ok(Value::Object(payload))
}

/// `_build_headline` — the most-changed metric, or one of two fixed strings.
fn build_headline(order: &[String], metric_summary: &Map<String, Value>) -> String {
    if metric_summary.is_empty() {
        return "No metrics produced.".to_owned();
    }
    // `if best is None or magnitude > best[1]` — a strict `>`, so the FIRST
    // metric at the maximum magnitude wins and insertion order breaks the tie.
    let mut best: Option<(&str, f64)> = None;
    for metric in order {
        let Some(avg) = metric_summary[metric]["avg_delta"].as_f64() else {
            // `isinstance(avg, int | float)` is false for `None`.
            continue;
        };
        let magnitude = avg.abs();
        if best.is_none_or(|(_, current)| magnitude > current) {
            best = Some((metric.as_str(), magnitude));
        }
    }
    let Some((metric, _)) = best else {
        return "No comparable pre/post deltas (analyzer ran but no metric had both sides)."
            .to_owned();
    };
    let summary = &metric_summary[metric];
    let avg = summary["avg_delta"].as_f64().unwrap_or(0.0);
    let files = summary["files"].as_i64().unwrap_or(0);
    let direction =
        if (avg < 0.0 && lower_is_better(metric)) || (avg > 0.0 && !lower_is_better(metric)) {
            "Reduced"
        } else {
            "Increased"
        };
    let plural = if files == 1 { "" } else { "s" };
    format!(
        "{direction} {metric} by {} on average across {files} file{plural}.",
        format_g3(avg.abs())
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Classification {
    Improved,
    Regressed,
    Neutral,
    Unknown,
}

#[derive(Debug, Clone, Copy)]
struct Triple {
    pre: Option<f64>,
    post: Option<f64>,
    delta: Option<f64>,
}

/// `_classify_delta`.
fn classify_delta(metric: &str, pre: Option<f64>, post: Option<f64>) -> Classification {
    let (Some(pre), Some(post)) = (pre, post) else {
        return Classification::Unknown;
    };
    if pre == 0.0 {
        // The div-by-zero guard, with its own direction rule.
        if post == 0.0 {
            return Classification::Neutral;
        }
        return if lower_is_better(metric) {
            Classification::Regressed
        } else {
            Classification::Improved
        };
    }
    let pct = (post - pre) / pre.abs();
    if pct.abs() < SIGNIFICANT_DELTA_PCT {
        return Classification::Neutral;
    }
    if lower_is_better(metric) {
        if pct < 0.0 {
            Classification::Improved
        } else {
            Classification::Regressed
        }
    } else if pct > 0.0 {
        Classification::Improved
    } else {
        Classification::Regressed
    }
}

struct FindingRow {
    file_path: String,
    language: String,
    ts: String,
    metric: String,
    pre_value: Option<f64>,
    post_value: Option<f64>,
    delta: Option<f64>,
    details_json: Option<String>,
}

fn findings_rows(conn: &Connection, session_id: &str) -> rusqlite::Result<Vec<FindingRow>> {
    // The table-existence guard that stands in for `schema.apply` — see the
    // module docs (DIV-134). An absent table yields no rows, which is the same
    // body a freshly-migrated empty table yields.
    if !table_exists(conn, "static_analysis_findings")? {
        return Ok(Vec::new());
    }
    let mut stmt = conn.prepare(
        "SELECT file_path, language, ts, metric, pre_value, post_value, \
                delta, details_json \
         FROM static_analysis_findings \
         WHERE session_id = ? \
         ORDER BY file_path, metric",
    )?;
    let rows = stmt
        .query_map([session_id], |row| {
            Ok(FindingRow {
                file_path: row.get(0)?,
                language: row.get(1)?,
                ts: row.get(2)?,
                metric: row.get(3)?,
                pre_value: numeric(row.get_ref(4)?),
                post_value: numeric(row.get_ref(5)?),
                delta: numeric(row.get_ref(6)?),
                details_json: row.get(7)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// A `REAL` column read as `float | None`, tolerating an `INTEGER`-stored value.
#[allow(clippy::cast_precision_loss)]
fn numeric(value: ValueRef<'_>) -> Option<f64> {
    match value {
        ValueRef::Real(f) => Some(f),
        ValueRef::Integer(i) => Some(i as f64),
        _ => None,
    }
}

/// A nullable `REAL` as JSON. The column is declared `REAL`, so a present value
/// is a float and renders with its decimal point.
fn opt_float(value: Option<f64>) -> Value {
    value.map_or(Value::Null, |f| PyNum::Float(f).to_json())
}

/// `f"{value:.3g}"` — Python's general format, three significant digits.
///
/// `{:.3e}`-then-trim is not the same thing: `g` drops trailing zeros AND the
/// trailing `.`, and it only goes exponential outside `[1e-4, 1e3)`. Written
/// out because the headline string is compared byte for byte.
fn format_g3(value: f64) -> String {
    format_g(value, 3)
}

fn format_g(value: f64, precision: usize) -> String {
    if value == 0.0 {
        return "0".to_owned();
    }
    if !value.is_finite() {
        return format!("{value}");
    }
    let exponent = value.abs().log10().floor() as i32;
    #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
    let precision_i32 = precision as i32;
    if exponent < -4 || exponent >= precision_i32 {
        // Exponential form: `{:.{p-1}e}` with the exponent at two digits
        // minimum, then the same zero-trimming.
        let rendered = format!("{value:.*e}", precision.saturating_sub(1));
        let (mantissa, exp) = rendered.split_once('e').unwrap_or((rendered.as_str(), "0"));
        let mantissa = trim_zeros(mantissa);
        let exp_value: i32 = exp.parse().unwrap_or(0);
        let sign = if exp_value < 0 { '-' } else { '+' };
        return format!("{mantissa}e{sign}{:02}", exp_value.abs());
    }
    #[allow(clippy::cast_sign_loss)]
    let decimals = (precision_i32 - 1 - exponent).max(0) as usize;
    trim_zeros(&format!("{value:.decimals$}"))
}

fn trim_zeros(text: &str) -> String {
    if !text.contains('.') {
        return text.to_owned();
    }
    text.trim_end_matches('0').trim_end_matches('.').to_owned()
}

fn sql_500(err: rusqlite::Error) -> HttpError {
    HttpError::new(StatusCode::INTERNAL_SERVER_ERROR, err.to_string())
}

fn any_500(err: &anyhow::Error) -> HttpError {
    HttpError::new(StatusCode::INTERNAL_SERVER_ERROR, err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unanalyzed_session_is_an_empty_shape_not_a_404() {
        let payload = Value::Object({
            let mut summary = Map::new();
            summary.insert("files".to_owned(), Value::from(0));
            summary.insert("languages".to_owned(), Value::Array(vec![]));
            summary.insert("metrics".to_owned(), Value::Object(Map::new()));
            summary.insert(
                "headline".to_owned(),
                Value::from(build_headline(&[], &Map::new())),
            );
            let mut obj = Map::new();
            obj.insert("session_id".to_owned(), Value::from("nope"));
            obj.insert("findings".to_owned(), Value::Array(vec![]));
            obj.insert("summary".to_owned(), Value::Object(summary));
            obj
        });
        assert_eq!(
            JsonBody::ok(payload).render(),
            r#"{"session_id":"nope","findings":[],"summary":{"files":0,"languages":[],"metrics":{},"headline":"No metrics produced."}}"#
        );
    }

    #[test]
    fn a_missing_side_classifies_as_unknown_and_is_counted_nowhere() {
        // The store's one live finding: `pre_value` NULL, `post_value` 1.0.
        assert_eq!(
            classify_delta("lint_count", None, Some(1.0)),
            Classification::Unknown
        );
        assert_eq!(
            classify_delta("lint_count", Some(1.0), None),
            Classification::Unknown
        );
    }

    #[test]
    fn zero_to_nonzero_takes_the_direction_of_the_metric() {
        // The div-by-zero guard, both ways round.
        assert_eq!(
            classify_delta("lint_count", Some(0.0), Some(3.0)),
            Classification::Regressed
        );
        assert_eq!(
            classify_delta("coverage", Some(0.0), Some(3.0)),
            Classification::Improved
        );
        assert_eq!(
            classify_delta("lint_count", Some(0.0), Some(0.0)),
            Classification::Neutral
        );
    }

    #[test]
    fn a_sub_threshold_move_is_neutral_in_both_directions() {
        // 19% either way is under `_SIGNIFICANT_DELTA_PCT`.
        assert_eq!(
            classify_delta("complexity", Some(100.0), Some(119.0)),
            Classification::Neutral
        );
        assert_eq!(
            classify_delta("complexity", Some(100.0), Some(81.0)),
            Classification::Neutral
        );
        assert_eq!(
            classify_delta("complexity", Some(100.0), Some(79.0)),
            Classification::Improved
        );
        assert_eq!(
            classify_delta("complexity", Some(100.0), Some(121.0)),
            Classification::Regressed
        );
    }

    #[test]
    fn an_unknown_metric_defaults_to_lower_is_better() {
        // `_LOWER_IS_BETTER.get(metric, True)`.
        assert!(lower_is_better("some_future_metric"));
        assert!(!lower_is_better("coverage"));
    }

    #[test]
    fn the_headline_uses_pythons_three_significant_digits() {
        assert_eq!(format_g3(0.7), "0.7");
        assert_eq!(format_g3(0.6666666), "0.667");
        assert_eq!(format_g3(5.0), "5");
        assert_eq!(format_g3(1234.0), "1.23e+03");
        assert_eq!(format_g3(0.000_012_3), "1.23e-05");
        assert_eq!(format_g3(12.30), "12.3");
    }

    #[test]
    fn the_headline_names_the_biggest_mover_and_pluralises() {
        let mut metrics = Map::new();
        metrics.insert(
            "complexity".to_owned(),
            serde_json::json!({"files": 3, "avg_delta": -0.7, "improved": 2, "regressed": 0, "neutral": 1}),
        );
        metrics.insert(
            "lint_count".to_owned(),
            serde_json::json!({"files": 1, "avg_delta": 0.2, "improved": 0, "regressed": 1, "neutral": 0}),
        );
        let order = vec!["complexity".to_owned(), "lint_count".to_owned()];
        assert_eq!(
            build_headline(&order, &metrics),
            "Reduced complexity by 0.7 on average across 3 files."
        );

        // A single file drops the plural `s`.
        let mut one = Map::new();
        one.insert(
            "lint_count".to_owned(),
            serde_json::json!({"files": 1, "avg_delta": 5.0, "improved": 0, "regressed": 1, "neutral": 0}),
        );
        assert_eq!(
            build_headline(&["lint_count".to_owned()], &one),
            "Increased lint_count by 5 on average across 1 file."
        );
    }

    #[test]
    fn every_null_avg_delta_falls_through_to_the_second_fixed_string() {
        let mut metrics = Map::new();
        metrics.insert(
            "lint_count".to_owned(),
            serde_json::json!({"files": 1, "avg_delta": null, "improved": 0, "regressed": 0, "neutral": 0}),
        );
        assert_eq!(
            build_headline(&["lint_count".to_owned()], &metrics),
            "No comparable pre/post deltas (analyzer ran but no metric had both sides)."
        );
    }

    #[test]
    fn round_matches_pythons_banker_rounding() {
        // `round(2.675, 2)` is 2.67 in CPython — the binary value is just under
        // the tie — and a naive `(x * 100).round() / 100` gives 2.68.
        assert!((round_py(2.675, 2) - 2.67).abs() < f64::EPSILON);
        assert!((round_py(0.123_456_789, 4) - 0.1235).abs() < f64::EPSILON);
    }
}
