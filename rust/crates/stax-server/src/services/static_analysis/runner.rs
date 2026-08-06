//! `static_analysis/runner.py` — orchestration: reconstruct, analyze, persist.
//!
//! One session at a time: Playback v2 rebuilds the pre snapshot (at
//! `first_ts`) and the post snapshot (at `last_ts`), every touched file runs
//! through its language's analyzer twice, and one row per (file, metric)
//! lands in `static_analysis_findings` via INSERT OR REPLACE. The reference's
//! backfill fans sessions across a thread pool with one connection per
//! worker; `std::thread::scope` carries that here, factory and all.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use rusqlite::Connection;
use serde_json::{Map, Value, json};
use stax_core::queries::pytime;
use stax_etl::stats::aggregator::round_py;

use super::{FileMetrics, go_analyzer, python_analyzer, typescript_analyzer};
use crate::services::playback_fs::{self, ReconstructError};

/// `METRIC_KEYS` — the closed enum the schema accepts; a metric outside this
/// set is never persisted, so a typo can't quietly write rubbish.
pub const METRIC_KEYS: [&str; 4] = ["complexity", "coverage", "lint_count", "type_completeness"];

/// `SUPPORTED_LANGUAGES`, in `_LANGUAGE_TABLE` insertion order.
pub const SUPPORTED_LANGUAGES: [&str; 3] = ["python", "typescript", "go"];

/// `_DETAILS_JSON_CAP` — the defensive blob cap.
const DETAILS_JSON_CAP: usize = 4_000;

/// `AnalysisOutcome` — per-session summary returned by [`analyze_session`].
#[derive(Debug, Clone, Default)]
pub struct AnalysisOutcome {
    pub session_id: String,
    pub files_analyzed: i64,
    pub rows_written: i64,
    pub languages: Vec<String>,
    pub warnings: Vec<String>,
    pub skipped_files: Vec<String>,
}

impl AnalysisOutcome {
    /// `outcome_to_dict` — `dataclasses.asdict`, declaration order.
    #[must_use]
    pub fn to_dict(&self) -> Map<String, Value> {
        let mut map = Map::new();
        map.insert("session_id".into(), json!(self.session_id));
        map.insert("files_analyzed".into(), json!(self.files_analyzed));
        map.insert("rows_written".into(), json!(self.rows_written));
        map.insert("languages".into(), json!(self.languages));
        map.insert("warnings".into(), json!(self.warnings));
        map.insert("skipped_files".into(), json!(self.skipped_files));
        map
    }
}

/// `SessionQuality` — the shape [`get_session_quality`] returns.
#[derive(Debug, Clone, Default)]
pub struct SessionQuality {
    pub session_id: String,
    pub findings: Vec<Map<String, Value>>,
    pub summary: Map<String, Value>,
}

impl SessionQuality {
    /// `quality_to_dict` — `dataclasses.asdict`, declaration order.
    #[must_use]
    pub fn to_dict(&self) -> Map<String, Value> {
        let mut map = Map::new();
        map.insert("session_id".into(), json!(self.session_id));
        map.insert(
            "findings".into(),
            Value::Array(self.findings.iter().cloned().map(Value::Object).collect()),
        );
        map.insert("summary".into(), Value::Object(self.summary.clone()));
        map
    }
}

/// `detect_language` — suffix-based, `None` ⇒ skipped silently.
#[must_use]
pub fn detect_language(file_path: &str) -> Option<&'static str> {
    let suffix = Path::new(file_path)
        .extension()
        .map(|ext| format!(".{}", ext.to_string_lossy().to_lowercase()))
        .unwrap_or_default();
    match suffix.as_str() {
        ".py" => Some("python"),
        ".ts" | ".tsx" | ".js" | ".jsx" => Some("typescript"),
        ".go" => Some("go"),
        _ => None,
    }
}

fn analyzer_available(language: &str) -> (bool, String) {
    match language {
        "python" => python_analyzer::available(),
        "typescript" => typescript_analyzer::available(),
        _ => go_analyzer::available(),
    }
}

fn analyzer_analyze(language: &str, path: &Path, content: &str) -> FileMetrics {
    match language {
        "python" => python_analyzer::analyze(path, content),
        "typescript" => typescript_analyzer::analyze(path, content),
        _ => go_analyzer::analyze(path, content),
    }
}

/// `_session_bounds` — `(first_ts, last_ts)` off the indexed `sessions` row.
fn session_bounds(conn: &Connection, session_id: &str) -> Option<(String, String)> {
    let row: Option<(Option<String>, Option<String>)> = conn
        .query_row(
            "SELECT first_ts, last_ts FROM sessions WHERE session_id = ? LIMIT 1",
            [session_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .ok();
    let (first, last) = row?;
    let first = first.filter(|value| !value.is_empty())?;
    let last = last.filter(|value| !value.is_empty())?;
    Some((first, last))
}

/// `_reconstruct_snapshots` — `(pre_files, post_files, warnings)`.
fn reconstruct_snapshots(
    conn: &Connection,
    session_id: &str,
) -> (
    BTreeMap<String, String>,
    BTreeMap<String, String>,
    Vec<String>,
) {
    let Some((first_ts, last_ts)) = session_bounds(conn, session_id) else {
        return (
            BTreeMap::new(),
            BTreeMap::new(),
            vec![format!("session has no first_ts/last_ts: {session_id}")],
        );
    };

    let pre = playback_fs::reconstruct_fs_at(conn, session_id, &first_ts, None, true);
    let post = playback_fs::reconstruct_fs_at(conn, session_id, &last_ts, None, true);
    let (pre, post) = match (pre, post) {
        (Ok(pre), Ok(post)) => (pre, post),
        (Err(err), _) | (_, Err(err)) => {
            let text = match err {
                ReconstructError::UnknownSession(text) => text,
                other => format!("playback_fs error: {}", other.detail()),
            };
            return (BTreeMap::new(), BTreeMap::new(), vec![text]);
        }
    };

    let files_of = |payload: &Value| -> BTreeMap<String, String> {
        payload
            .get("files")
            .and_then(Value::as_object)
            .map(|files| {
                files
                    .iter()
                    .map(|(path, file)| {
                        let content = file
                            .get("content")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned();
                        (path.clone(), content)
                    })
                    .collect()
            })
            .unwrap_or_default()
    };
    let warnings = post
        .get("warnings")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .map(|w| w.as_str().unwrap_or_default().to_owned())
                .collect()
        })
        .unwrap_or_default();
    (files_of(&pre), files_of(&post), warnings)
}

/// `_write_temp` — a unique tmp file without `mkstemp`: pid + an atomic
/// counter give uniqueness, `create_new` gives the collision-refusal
/// `mkstemp` promises. (No `tempfile` crate in the workspace; not worth
/// adding for one call site.)
fn write_temp(content: &str, suffix: &str) -> std::io::Result<PathBuf> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    loop {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("sa_{}_{n}{suffix}", std::process::id()));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut file) => {
                use std::io::Write as _;
                file.write_all(content.as_bytes())?;
                return Ok(path);
            }
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(err) => return Err(err),
        }
    }
}

/// `_analyze_file_content` — tmp write, analyze, always clean up.
fn analyze_file_content(
    language: &str,
    file_path: &str,
    content: &str,
) -> Result<FileMetrics, String> {
    let suffix = Path::new(file_path)
        .extension()
        .map_or(".tmp".to_owned(), |ext| {
            format!(".{}", ext.to_string_lossy())
        });
    let tmp =
        write_temp(content, &suffix).map_err(|err| format!("analyzer_error: OSError: {err}"))?;
    let metrics = analyzer_analyze(language, &tmp, content);
    let _ = std::fs::remove_file(&tmp);
    Ok(metrics)
}

/// `_safe_json_dumps` — `json.dumps(obj, default=str)` under the cap.
fn safe_json_dumps(obj: &Value) -> String {
    let text = stax_memory::pyjson::dumps_py_default(obj);
    if text.len() > DETAILS_JSON_CAP {
        let mut cut = DETAILS_JSON_CAP - 16;
        while cut > 0 && !text.is_char_boundary(cut) {
            cut -= 1;
        }
        return format!("{}...[truncated]\"}}", &text[..cut]);
    }
    text
}

/// One `static_analysis_findings` row.
#[derive(Debug, Clone)]
struct FindingRow {
    session_id: String,
    file_path: String,
    language: String,
    ts: String,
    metric: String,
    pre_value: Option<f64>,
    post_value: Option<f64>,
    delta: Option<f64>,
    details_json: Option<String>,
}

/// `_build_finding_rows` — one row per metric either side produced, or the
/// documented placeholder row when neither side produced anything.
#[allow(clippy::too_many_lines)]
fn build_finding_rows(
    session_id: &str,
    file_path: &str,
    language: &str,
    pre: Option<&FileMetrics>,
    post: Option<&FileMetrics>,
    pre_missing_reason: Option<&str>,
    post_missing_reason: Option<&str>,
) -> Vec<FindingRow> {
    let mut metrics_seen: BTreeSet<String> = BTreeSet::new();
    if let Some(pre) = pre {
        metrics_seen.extend(pre.metrics.keys().cloned());
    }
    if let Some(post) = post {
        metrics_seen.extend(post.metrics.keys().cloned());
    }

    let ts_now = pytime::isoformat_utc(pytime::now_micros());
    let mut rows = Vec::new();

    if metrics_seen.is_empty() && (pre_missing_reason.is_some() || post_missing_reason.is_some()) {
        let details = json!({
            "reason": "no_metrics_produced",
            "pre_reason": pre_missing_reason,
            "post_reason": post_missing_reason,
        });
        rows.push(FindingRow {
            session_id: session_id.to_owned(),
            file_path: file_path.to_owned(),
            language: language.to_owned(),
            ts: ts_now,
            metric: "lint_count".to_owned(),
            pre_value: None,
            post_value: None,
            delta: None,
            details_json: Some(safe_json_dumps(&details)),
        });
        return rows;
    }

    // BTreeSet iterates sorted — `for metric in sorted(metrics_seen)`.
    for metric in &metrics_seen {
        if !METRIC_KEYS.contains(&metric.as_str()) {
            continue;
        }
        let side = |m: Option<&FileMetrics>| {
            m.and_then(|m| m.metrics.get(metric))
                .and_then(Value::as_f64)
        };
        let pre_val = side(pre);
        let post_val = side(post);
        let delta = match (pre_val, post_val) {
            (Some(a), Some(b)) => Some(round_py(b - a, 6)),
            _ => None,
        };
        let mut details = Map::new();
        if let Some(extra) = pre.and_then(|m| m.details.get(metric)) {
            details.insert("pre".into(), extra.clone());
        }
        if let Some(extra) = post.and_then(|m| m.details.get(metric)) {
            details.insert("post".into(), extra.clone());
        }
        if pre_val.is_none() {
            if let Some(reason) = pre_missing_reason {
                details.insert("pre_reason".into(), json!(reason));
            } else if post_val.is_some() {
                details.insert(
                    "pre_reason".into(),
                    json!("metric_not_produced_for_pre_state"),
                );
            }
        }
        if post_val.is_none() {
            if let Some(reason) = post_missing_reason {
                details.insert("post_reason".into(), json!(reason));
            } else if pre_val.is_some() {
                details.insert(
                    "post_reason".into(),
                    json!("metric_not_produced_for_post_state"),
                );
            }
        }
        let details_json = if details.is_empty() {
            None
        } else {
            Some(safe_json_dumps(&Value::Object(details)))
        };
        rows.push(FindingRow {
            session_id: session_id.to_owned(),
            file_path: file_path.to_owned(),
            language: language.to_owned(),
            ts: ts_now.clone(),
            metric: metric.clone(),
            pre_value: pre_val,
            post_value: post_val,
            delta,
            details_json,
        });
    }
    rows
}

/// `_persist_rows` — INSERT OR REPLACE, idempotent on (session, file, metric).
fn persist_rows(conn: &Connection, rows: &[FindingRow]) -> rusqlite::Result<i64> {
    if rows.is_empty() {
        return Ok(0);
    }
    let mut statement = conn.prepare(
        "INSERT OR REPLACE INTO static_analysis_findings \
           (session_id, file_path, language, ts, metric, \
            pre_value, post_value, delta, details_json) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
    )?;
    for row in rows {
        statement.execute(rusqlite::params![
            row.session_id,
            row.file_path,
            row.language,
            row.ts,
            row.metric,
            row.pre_value,
            row.post_value,
            row.delta,
            row.details_json,
        ])?;
    }
    Ok(rows.len() as i64)
}

/// `analyze_session` — the whole pass for one session.
///
/// # Errors
/// An empty `session_id` (the reference's `ValueError`) or a SQLite failure
/// from the final persist.
pub fn analyze_session(
    conn: &Connection,
    session_id: &str,
    only_languages: Option<&[String]>,
) -> Result<AnalysisOutcome, String> {
    if session_id.trim().is_empty() {
        return Err("session_id must be non-empty".to_owned());
    }

    let (pre_files, post_files, mut warnings) = reconstruct_snapshots(conn, session_id);

    let mut candidate_paths: BTreeSet<String> = pre_files.keys().cloned().collect();
    candidate_paths.extend(post_files.keys().cloned());

    let mut languages_seen: BTreeSet<String> = BTreeSet::new();
    let mut rows_to_write = Vec::new();
    let mut skipped = Vec::new();
    let mut files_analyzed = 0i64;

    for path in &candidate_paths {
        let Some(language) = detect_language(path) else {
            skipped.push(format!("{path}: unsupported language"));
            continue;
        };
        if let Some(only) = only_languages
            && !only.iter().any(|l| l == language)
        {
            continue;
        }
        let (avail, why) = analyzer_available(language);
        if !avail {
            warnings.push(format!("{language}: skipped — {why}"));
            skipped.push(format!("{path}: {language} analyzer unavailable"));
            continue;
        }
        files_analyzed += 1;
        languages_seen.insert(language.to_owned());

        let pre_content = pre_files.get(path);
        let post_content = post_files.get(path);
        if let (Some(a), Some(b)) = (pre_content, post_content)
            && a == b
        {
            continue;
        }

        let mut pre_metrics = None;
        let mut post_metrics = None;
        let mut pre_missing_reason: Option<String> = None;
        let mut post_missing_reason: Option<String> = None;

        match pre_content {
            None => pre_missing_reason = Some("file_created_in_session".to_owned()),
            Some(content) => match analyze_file_content(language, path, content) {
                Ok(metrics) => pre_metrics = Some(metrics),
                Err(reason) => pre_missing_reason = Some(reason),
            },
        }
        match post_content {
            None => post_missing_reason = Some("file_deleted_in_session".to_owned()),
            Some(content) => match analyze_file_content(language, path, content) {
                Ok(metrics) => post_metrics = Some(metrics),
                Err(reason) => post_missing_reason = Some(reason),
            },
        }

        rows_to_write.extend(build_finding_rows(
            session_id,
            path,
            language,
            pre_metrics.as_ref(),
            post_metrics.as_ref(),
            pre_missing_reason.as_deref(),
            post_missing_reason.as_deref(),
        ));
        if let Some(metrics) = &pre_metrics {
            warnings.extend(metrics.warnings.iter().cloned());
        }
        if let Some(metrics) = &post_metrics {
            warnings.extend(metrics.warnings.iter().cloned());
        }
    }

    let rows_written = persist_rows(conn, &rows_to_write).map_err(|err| err.to_string())?;

    Ok(AnalysisOutcome {
        session_id: session_id.to_owned(),
        files_analyzed,
        rows_written,
        languages: languages_seen.into_iter().collect(),
        warnings,
        skipped_files: skipped,
    })
}

/// `_sessions_lacking_findings`.
fn sessions_lacking_findings(
    conn: &Connection,
    since: Option<&str>,
    limit: Option<i64>,
) -> rusqlite::Result<Vec<String>> {
    let mut sql = String::from(
        "SELECT s.session_id FROM sessions s WHERE NOT EXISTS (\
           SELECT 1 FROM static_analysis_findings f \
           WHERE f.session_id = s.session_id)",
    );
    let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
    if let Some(since) = since {
        sql.push_str(" AND s.last_ts >= ?");
        params.push(Box::new(since.to_owned()));
    }
    sql.push_str(" ORDER BY s.last_ts DESC");
    if let Some(limit) = limit
        && limit > 0
    {
        sql.push_str(" LIMIT ?");
        params.push(Box::new(limit));
    }
    let mut statement = conn.prepare(&sql)?;
    let refs: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(AsRef::as_ref).collect();
    let rows = statement.query_map(refs.as_slice(), |row| row.get::<_, String>(0))?;
    rows.collect()
}

/// `_default_concurrency` — `min(4, cpu_count)`.
fn default_concurrency() -> usize {
    std::thread::available_parallelism()
        .map_or(1, |n| n.get().min(4))
        .max(1)
}

/// `backfill` — analyze every session lacking findings.
///
/// The concurrent path opens one connection per worker via `factory`, as the
/// reference requires of sqlite across threads. `factory: None` ⇒ the
/// single-threaded path on `conn`, exactly the reference's fallback.
#[must_use]
pub fn backfill(
    conn: &Connection,
    since: Option<&str>,
    limit: Option<i64>,
    concurrency: Option<usize>,
    factory: Option<&(dyn Fn() -> Option<Connection> + Sync)>,
) -> Map<String, Value> {
    let concurrency = concurrency.unwrap_or_else(default_concurrency).max(1);
    let candidates = sessions_lacking_findings(conn, since, limit).unwrap_or_default();
    let report = |candidates: usize, analyzed: i64, rows: i64, warnings: i64| {
        let mut map = Map::new();
        map.insert("candidates".into(), json!(candidates));
        map.insert("analyzed".into(), json!(analyzed));
        map.insert("rows_written".into(), json!(rows));
        map.insert("warnings_count".into(), json!(warnings));
        map
    };
    if candidates.is_empty() {
        return report(0, 0, 0, 0);
    }

    if concurrency == 1 || factory.is_none() {
        let mut analyzed = 0;
        let mut rows_total = 0;
        let mut warn_total = 0;
        for sid in &candidates {
            let Ok(outcome) = analyze_session(conn, sid, None) else {
                continue;
            };
            analyzed += 1;
            rows_total += outcome.rows_written;
            warn_total += outcome.warnings.len() as i64;
        }
        return report(candidates.len(), analyzed, rows_total, warn_total);
    }

    let factory = factory.expect("checked above");
    let outcomes = std::sync::Mutex::new(Vec::new());
    let queue = std::sync::Mutex::new(candidates.clone().into_iter());
    std::thread::scope(|scope| {
        for _ in 0..concurrency {
            scope.spawn(|| {
                loop {
                    let Some(sid) = queue.lock().ok().and_then(|mut it| it.next()) else {
                        return;
                    };
                    let Some(worker_conn) = factory() else {
                        continue;
                    };
                    if let Ok(outcome) = analyze_session(&worker_conn, &sid, None)
                        && let Ok(mut sink) = outcomes.lock()
                    {
                        sink.push(outcome);
                    }
                }
            });
        }
    });
    let outcomes = outcomes.into_inner().unwrap_or_default();
    let analyzed = outcomes.len() as i64;
    let rows_total = outcomes.iter().map(|o| o.rows_written).sum();
    let warn_total = outcomes.iter().map(|o| o.warnings.len() as i64).sum();
    report(candidates.len(), analyzed, rows_total, warn_total)
}

/// `_SIGNIFICANT_DELTA_PCT`.
const SIGNIFICANT_DELTA_PCT: f64 = 0.20;

fn lower_is_better(metric: &str) -> bool {
    !matches!(metric, "coverage" | "type_completeness")
}

/// `_classify_delta`.
fn classify_delta(metric: &str, pre: Option<f64>, post: Option<f64>) -> &'static str {
    let (Some(pre), Some(post)) = (pre, post) else {
        return "unknown";
    };
    if pre == 0.0 {
        if post == 0.0 {
            return "neutral";
        }
        return if lower_is_better(metric) {
            "regressed"
        } else {
            "improved"
        };
    }
    let pct = (post - pre) / pre.abs();
    if pct.abs() < SIGNIFICANT_DELTA_PCT {
        return "neutral";
    }
    if lower_is_better(metric) {
        if pct < 0.0 { "improved" } else { "regressed" }
    } else if pct > 0.0 {
        "improved"
    } else {
        "regressed"
    }
}

/// `get_session_quality` — findings + aggregate summary.
///
/// # Errors
/// A SQLite failure reading the findings table.
/// `(pre_value, post_value, delta)` — the reference's per-row triple.
type MetricTriple = (Option<f64>, Option<f64>, Option<f64>);

pub fn get_session_quality(
    conn: &Connection,
    session_id: &str,
) -> rusqlite::Result<SessionQuality> {
    let mut statement = conn.prepare(
        "SELECT file_path, language, ts, metric, pre_value, post_value, \
                delta, details_json \
         FROM static_analysis_findings \
         WHERE session_id = ? \
         ORDER BY file_path, metric",
    )?;
    struct Raw {
        file_path: String,
        language: String,
        ts: Value,
        metric: String,
        pre_value: Option<f64>,
        post_value: Option<f64>,
        delta: Option<f64>,
        details_json: Value,
    }
    let raws: Vec<Raw> = statement
        .query_map([session_id], |row| {
            Ok(Raw {
                file_path: row.get(0)?,
                language: row.get(1)?,
                ts: row
                    .get::<_, Option<String>>(2)?
                    .map_or(Value::Null, Value::from),
                metric: row.get(3)?,
                pre_value: row.get(4)?,
                post_value: row.get(5)?,
                delta: row.get(6)?,
                details_json: row
                    .get::<_, Option<String>>(7)?
                    .map_or(Value::Null, Value::from),
            })
        })?
        .collect::<rusqlite::Result<_>>()?;

    let mut findings = Vec::new();
    let mut by_metric: BTreeMap<String, Vec<MetricTriple>> = BTreeMap::new();
    let mut languages: BTreeSet<String> = BTreeSet::new();
    let mut file_paths: BTreeSet<String> = BTreeSet::new();
    for raw in &raws {
        let mut finding = Map::new();
        finding.insert("file_path".into(), json!(raw.file_path));
        finding.insert("language".into(), json!(raw.language));
        finding.insert("ts".into(), raw.ts.clone());
        finding.insert("metric".into(), json!(raw.metric));
        finding.insert(
            "pre_value".into(),
            raw.pre_value.map_or(Value::Null, Value::from),
        );
        finding.insert(
            "post_value".into(),
            raw.post_value.map_or(Value::Null, Value::from),
        );
        finding.insert("delta".into(), raw.delta.map_or(Value::Null, Value::from));
        finding.insert("details_json".into(), raw.details_json.clone());
        findings.push(finding);
        languages.insert(raw.language.clone());
        file_paths.insert(raw.file_path.clone());
        by_metric.entry(raw.metric.clone()).or_default().push((
            raw.pre_value,
            raw.post_value,
            raw.delta,
        ));
    }

    let mut metric_summary = Map::new();
    for (metric, triples) in &by_metric {
        let observed: Vec<f64> = triples.iter().filter_map(|t| t.2).collect();
        let avg_delta = if observed.is_empty() {
            None
        } else {
            Some(observed.iter().sum::<f64>() / observed.len() as f64)
        };
        let count_kind = |kind: &str| -> i64 {
            triples
                .iter()
                .filter(|t| classify_delta(metric, t.0, t.1) == kind)
                .count() as i64
        };
        let mut entry = Map::new();
        entry.insert("files".into(), json!(triples.len()));
        entry.insert(
            "avg_delta".into(),
            avg_delta.map_or(Value::Null, |avg| Value::from(round_py(avg, 4))),
        );
        entry.insert("improved".into(), json!(count_kind("improved")));
        entry.insert("regressed".into(), json!(count_kind("regressed")));
        entry.insert("neutral".into(), json!(count_kind("neutral")));
        metric_summary.insert(metric.clone(), Value::Object(entry));
    }

    let mut summary = Map::new();
    summary.insert("files".into(), json!(file_paths.len()));
    summary.insert(
        "languages".into(),
        json!(languages.iter().collect::<Vec<_>>()),
    );
    let headline = build_headline(&metric_summary);
    summary.insert("metrics".into(), Value::Object(metric_summary));
    summary.insert("headline".into(), json!(headline));

    Ok(SessionQuality {
        session_id: session_id.to_owned(),
        findings,
        summary,
    })
}

/// `_build_headline` — the most-changed metric as one plain line.
fn build_headline(metric_summary: &Map<String, Value>) -> String {
    if metric_summary.is_empty() {
        return "No metrics produced.".to_owned();
    }
    let mut best: Option<(&str, f64, &Map<String, Value>)> = None;
    for (metric, entry) in metric_summary {
        let Some(entry) = entry.as_object() else {
            continue;
        };
        let Some(avg) = entry.get("avg_delta").and_then(Value::as_f64) else {
            continue;
        };
        let magnitude = avg.abs();
        if best.is_none_or(|(_, current, _)| magnitude > current) {
            best = Some((metric, magnitude, entry));
        }
    }
    let Some((metric, _, entry)) = best else {
        return "No comparable pre/post deltas (analyzer ran but no metric had both sides)."
            .to_owned();
    };
    let avg = entry
        .get("avg_delta")
        .and_then(Value::as_f64)
        .unwrap_or_default();
    let files = entry
        .get("files")
        .and_then(Value::as_i64)
        .unwrap_or_default();
    let direction =
        if (avg < 0.0 && lower_is_better(metric)) || (avg > 0.0 && !lower_is_better(metric)) {
            "Reduced"
        } else {
            "Increased"
        };
    // `f"{abs(avg):.3g}"` — CPython's general format with 3 significant digits.
    let magnitude = format!("{:.3}", avg.abs());
    let magnitude = py_general_3g(avg.abs(), &magnitude);
    format!(
        "{direction} {metric} by {magnitude} on average across {files} file{}.",
        if files == 1 { "" } else { "s" }
    )
}

/// `f"{x:.3g}"` — three significant digits, trailing zeros dropped, exponent
/// form past the thresholds CPython uses. The fallback argument keeps the
/// common path allocation-free.
fn py_general_3g(x: f64, _fallback: &str) -> String {
    let formatted = format!("{x:.3e}");
    let (mantissa, exponent) = formatted
        .split_once('e')
        .unwrap_or((formatted.as_str(), "0"));
    let exp: i32 = exponent.parse().unwrap_or(0);
    if (-4..3).contains(&exp) {
        let digits = (2 - exp).max(0);
        #[allow(clippy::cast_sign_loss)]
        let mut out = format!("{x:.*}", digits as usize);
        if out.contains('.') {
            while out.ends_with('0') {
                out.pop();
            }
            if out.ends_with('.') {
                out.pop();
            }
        }
        out
    } else {
        let mantissa = mantissa.trim_end_matches('0').trim_end_matches('.');
        format!(
            "{mantissa}e{}{:02}",
            if exp < 0 { '-' } else { '+' },
            exp.abs()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_language_matches_the_reference_table() {
        assert_eq!(detect_language("a/b.py"), Some("python"));
        assert_eq!(detect_language("a/B.TSX"), Some("typescript"));
        assert_eq!(detect_language("x.jsx"), Some("typescript"));
        assert_eq!(detect_language("m.go"), Some("go"));
        assert_eq!(detect_language("m.rs"), None);
        assert_eq!(detect_language("no_ext"), None);
    }

    #[test]
    fn classify_delta_matches_the_reference_cases() {
        assert_eq!(classify_delta("complexity", None, Some(1.0)), "unknown");
        assert_eq!(
            classify_delta("complexity", Some(0.0), Some(0.0)),
            "neutral"
        );
        assert_eq!(
            classify_delta("complexity", Some(0.0), Some(2.0)),
            "regressed"
        );
        assert_eq!(classify_delta("coverage", Some(0.0), Some(2.0)), "improved");
        assert_eq!(
            classify_delta("complexity", Some(10.0), Some(10.5)),
            "neutral"
        );
        assert_eq!(
            classify_delta("complexity", Some(10.0), Some(5.0)),
            "improved"
        );
        assert_eq!(
            classify_delta("coverage", Some(10.0), Some(5.0)),
            "regressed"
        );
    }

    #[test]
    fn finding_rows_emit_the_placeholder_when_nothing_was_produced() {
        let rows = build_finding_rows(
            "s",
            "f.py",
            "python",
            None,
            None,
            Some("file_created_in_session"),
            None,
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].metric, "lint_count");
        assert!(
            rows[0].pre_value.is_none() && rows[0].post_value.is_none() && rows[0].delta.is_none()
        );
        let details = rows[0].details_json.as_deref().unwrap();
        assert!(details.contains("no_metrics_produced"), "{details}");
    }

    #[test]
    fn finding_rows_skip_unknown_metrics_and_sort_known_ones() {
        let mut pre = FileMetrics::default();
        pre.metrics.insert("lint_count".into(), json!(3.0));
        pre.metrics.insert("made_up".into(), json!(1.0));
        let mut post = FileMetrics::default();
        post.metrics.insert("lint_count".into(), json!(1.0));
        post.metrics.insert("complexity".into(), json!(2.0));
        let rows = build_finding_rows("s", "f.py", "python", Some(&pre), Some(&post), None, None);
        let metrics: Vec<&str> = rows.iter().map(|r| r.metric.as_str()).collect();
        assert_eq!(
            metrics,
            ["complexity", "lint_count"],
            "sorted, made_up dropped"
        );
        assert_eq!(rows[1].delta, Some(-2.0));
        assert!(
            rows[0]
                .details_json
                .as_deref()
                .unwrap()
                .contains("metric_not_produced_for_pre_state")
        );
    }

    #[test]
    fn safe_json_dumps_caps_the_blob() {
        let big = json!({"k": "x".repeat(5000)});
        let text = safe_json_dumps(&big);
        assert!(text.len() <= DETAILS_JSON_CAP);
        assert!(text.ends_with("...[truncated]\"}"));
    }

    #[test]
    fn the_headline_reads_like_the_reference() {
        let mut summary = Map::new();
        let mut entry = Map::new();
        entry.insert("files".into(), json!(3));
        entry.insert("avg_delta".into(), json!(-0.7));
        summary.insert("complexity".into(), Value::Object(entry));
        assert_eq!(
            build_headline(&summary),
            "Reduced complexity by 0.7 on average across 3 files."
        );
        assert_eq!(build_headline(&Map::new()), "No metrics produced.");
    }
}
