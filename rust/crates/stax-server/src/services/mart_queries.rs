//! `store/mart_queries.py` — the ETL-mart read helpers the optimize surface needs.
//!
//! | Item | Python | Used by |
//! |---|---|---|
//! | [`table_exists`] | `_table_exists` | every helper below |
//! | [`iso_to_day`] | `_iso_to_day` | the day-keyed marts |
//! | [`mart_has_session_rows`] | same | `_detect_cache_overhead` |
//! | [`mart_has_tool_rows`] | same | the wave-5 short-circuits |
//! | [`mart_has_message_tool_rows`] | same | every per-message fast path |
//! | [`daily_global`] | same | `reports/anomaly._daily_cost_points` |
//! | [`session_mart_rows_for_compare`] | same | `reports/anomaly._session_cost_points` |
//! | [`session_mart_cache_overhead`] | same | `_detect_cache_overhead_from_mart` |
//! | [`tool_call_count_in_window`] | same | the `tool_mart` pre-flight checks |
//! | [`tool_mart_distinct_tool_names_in_window`] | same | `_detect_unused_mcp_servers` |
//! | [`message_tool_junk_reads`] | same | `_junk_reads_from_mart` |
//! | [`message_tool_read_edit_per_session`] | same | `_low_read_edit_from_mart` |
//! | [`message_tool_oversized`] | same | `_bash_output_from_mart` |
//! | [`message_tool_invoked_agents`] | same | `_detect_ghost_agents` |
//!
//! # What is load-bearing
//!
//! * **The "is the mart materialised?" gates are `SELECT 1 … LIMIT 1`, not a
//!   `COUNT(*)`,** and a *missing table* is `false` rather than an error. That is
//!   the whole empty-mart-falls-back-to-the-raw-scan contract: a fresh install
//!   with no ETL pass must keep working, so every detector asks one of these
//!   first and takes the slow path on `false`.
//! * **Every window filter is pushed as a `YYYY-MM-DD` *day* string,** sliced
//!   host-side from the caller's ISO-8601 bound (`_iso_to_day`). The marts are
//!   day-keyed and their indexes are on `(tool_name, day)`; turning the bound
//!   into `date(ts)` in SQL would make the index unusable. `_iso_to_day` returns
//!   `None` for anything shorter than ten characters, so a junk bound silently
//!   widens the window rather than erroring.
//! * **A slug filter becomes a `JOIN projects`, and an EMPTY slug list becomes
//!   no join at all** (`_norm_slugs` drops falsy entries; `if slugs` then
//!   decides). So an empty slug list means "all projects", not "no projects" —
//!   the opposite of the `project_ids` convention in `reports/prescribe.py`.
//!   Both are reproduced as written.
//! * **The row structs here are NARROWED to the columns the optimize surface
//!   reads.** Python's helpers `SELECT` the full mart row and hand back a
//!   `dict`; nothing downstream of `reports/anomaly.py` or `reports/optimize.py`
//!   looks at the rest, and a struct that names only the used columns is a
//!   compile-time record of that. The `SELECT` lists below are correspondingly
//!   shorter than Python's — invisible on the wire, and the one deliberate
//!   narrowing in this module.
//!
//! # Duplication, flagged rather than fixed
//!
//! `routes/cost.rs` carries private copies of [`table_exists`] and
//! [`mart_has_tool_rows`]. That file belongs to another batch and was not
//! touched; the overlap is DIV-119 on the integrator's dedup list.

use std::collections::HashSet;
use std::fmt::Write as _;

use rusqlite::Connection;
use rusqlite::types::Value as SqlValue;

/// `_table_exists` — `sqlite_master` with `type='table'`, tables only.
///
/// Note the asymmetry with `reports/prescribe.py::_table_exists`, which accepts
/// `type IN ('table','view')`. Two different guards in two different modules,
/// and both are ported where they live.
///
/// # Errors
/// Any SQLite error.
pub fn table_exists(conn: &Connection, name: &str) -> rusqlite::Result<bool> {
    let mut stmt =
        conn.prepare_cached("SELECT 1 FROM sqlite_master WHERE type='table' AND name=?")?;
    let mut rows = stmt.query([name])?;
    Ok(rows.next()?.is_some())
}

/// `_iso_to_day` — `YYYY-MM-DD` from an ISO-8601 stamp.
///
/// `if not iso_ts or len(iso_ts) < 10: return None` — a short or empty bound
/// yields `None`, which every caller reads as "no bound on this side". A junk
/// bound therefore *widens* the window silently instead of erroring.
///
/// The slice is by BYTE here and by CODE POINT in Python. Every caller passes a
/// `Scope` bound, which `parse_period` rendered as ASCII, so the two agree; a
/// non-ASCII bound cannot arise from any code path that reaches this.
#[must_use]
pub fn iso_to_day(iso_ts: Option<&str>) -> Option<String> {
    let iso_ts = iso_ts?;
    if iso_ts.len() < 10 {
        return None;
    }
    iso_ts.get(..10).map(str::to_owned)
}

/// `_norm_slugs` — drop falsy (here: empty) entries from a slug filter.
fn norm_slugs(project_slugs: Option<&[String]>) -> Vec<String> {
    project_slugs
        .unwrap_or_default()
        .iter()
        .filter(|s| !s.is_empty())
        .cloned()
        .collect()
}

/// `",".join("?" * n)`.
fn placeholders(n: usize) -> String {
    let mut out = String::with_capacity(n.saturating_mul(2));
    for i in 0..n {
        if i > 0 {
            out.push(',');
        }
        out.push('?');
    }
    out
}

/// `_push_day_window` — append an inclusive `[since, until]` day filter on `col`.
fn push_day_window(
    sql: &mut String,
    params: &mut Vec<SqlValue>,
    since_iso: Option<&str>,
    until_iso: Option<&str>,
    col: &str,
) {
    if let Some(day_from) = iso_to_day(since_iso) {
        let _ = write!(sql, " AND {col} >= ?");
        params.push(SqlValue::Text(day_from));
    }
    if let Some(day_to) = iso_to_day(until_iso) {
        let _ = write!(sql, " AND {col} <= ?");
        params.push(SqlValue::Text(day_to));
    }
}

// ── existence gates ──────────────────────────────────────────────────────────

/// `mart_has_session_rows` — `session_mart` has at least one row.
///
/// # Errors
/// Any SQLite error.
pub fn mart_has_session_rows(conn: &Connection) -> rusqlite::Result<bool> {
    has_any_row(conn, "session_mart")
}

/// `mart_has_tool_rows` — `tool_mart` has at least one row.
///
/// # Errors
/// Any SQLite error.
pub fn mart_has_tool_rows(conn: &Connection) -> rusqlite::Result<bool> {
    has_any_row(conn, "tool_mart")
}

/// `mart_has_message_tool_rows` — `message_tool_mart` has at least one row.
///
/// The gate `/api/optimize`'s `mart_empty` warning is built from, and the one
/// every per-message detector consults first.
///
/// # Errors
/// Any SQLite error.
pub fn mart_has_message_tool_rows(conn: &Connection) -> rusqlite::Result<bool> {
    has_any_row(conn, "message_tool_mart")
}

/// The shared body of the three gates: absent table → `false`, never an error.
fn has_any_row(conn: &Connection, table: &str) -> rusqlite::Result<bool> {
    if !table_exists(conn, table)? {
        return Ok(false);
    }
    let mut stmt = conn.prepare(&format!("SELECT 1 FROM {table} LIMIT 1"))?;
    let mut rows = stmt.query([])?;
    Ok(rows.next()?.is_some())
}

// ── daily_mart ───────────────────────────────────────────────────────────────

/// One `daily_mart` row, narrowed to what `reports/anomaly.py` reads.
#[derive(Debug, Clone)]
pub struct DailyGlobalRow {
    /// `day` — `YYYY-MM-DD`. `NULL` is possible and the caller skips it.
    pub day: Option<String>,
    /// `cost_usd`.
    pub cost_usd: Option<f64>,
}

/// `daily_global(conn, day_from=…, day_to=…)`, narrowed to `(day, cost_usd)`.
///
/// The provider/model filters exist in Python and no optimize caller passes
/// them, so they are not in this signature; adding them later is additive.
/// `ORDER BY day` is kept even though the only caller folds into a map and
/// re-sorts — it is what the reference runs.
///
/// # Errors
/// Any SQLite error other than a missing `daily_mart`, which is `[]`.
pub fn daily_global(
    conn: &Connection,
    day_from: Option<&str>,
    day_to: Option<&str>,
) -> rusqlite::Result<Vec<DailyGlobalRow>> {
    if !table_exists(conn, "daily_mart")? {
        return Ok(Vec::new());
    }
    let mut sql = "SELECT day, cost_usd FROM daily_mart WHERE 1=1".to_owned();
    let mut params: Vec<SqlValue> = Vec::new();
    // `if day_from:` — an EMPTY string is falsy in Python and adds no clause.
    if let Some(day_from) = day_from.filter(|v| !v.is_empty()) {
        sql.push_str(" AND day >= ?");
        params.push(SqlValue::Text(day_from.to_owned()));
    }
    if let Some(day_to) = day_to.filter(|v| !v.is_empty()) {
        sql.push_str(" AND day <= ?");
        params.push(SqlValue::Text(day_to.to_owned()));
    }
    sql.push_str(" ORDER BY day");
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(rusqlite::params_from_iter(params.iter()), |row| {
            Ok(DailyGlobalRow {
                day: row.get(0)?,
                cost_usd: row.get(1)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

// ── session_mart ─────────────────────────────────────────────────────────────

/// One `session_mart` row, narrowed to what `reports/anomaly.py` reads.
#[derive(Debug, Clone)]
pub struct SessionCompareRow {
    /// `session_id`.
    pub session_id: Option<String>,
    /// `provider`.
    pub provider: Option<String>,
    /// `primary_model`.
    pub primary_model: Option<String>,
    /// `first_ts`.
    pub first_ts: Option<String>,
    /// `message_count`.
    pub message_count: Option<i64>,
    /// `cost_usd`.
    pub cost_usd: Option<f64>,
}

/// `session_mart_rows_for_compare`, narrowed to the six columns anomaly reads.
///
/// The window is on `first_ts` — the session's START — so a session that began
/// before the window and ran into it is OUT, and one that began inside it and
/// ran past `until` is IN. That is the reference's rule and it is not obvious.
///
/// # Errors
/// Any SQLite error other than a missing `session_mart`, which is `[]`.
pub fn session_mart_rows_for_compare(
    conn: &Connection,
    since_iso: Option<&str>,
    until_iso: Option<&str>,
) -> rusqlite::Result<Vec<SessionCompareRow>> {
    if !table_exists(conn, "session_mart")? {
        return Ok(Vec::new());
    }
    let mut sql = "SELECT session_id, provider, primary_model, first_ts, \
                   message_count, cost_usd FROM session_mart WHERE 1=1"
        .to_owned();
    let mut params: Vec<SqlValue> = Vec::new();
    if let Some(since) = since_iso.filter(|v| !v.is_empty()) {
        sql.push_str(" AND first_ts >= ?");
        params.push(SqlValue::Text(since.to_owned()));
    }
    if let Some(until) = until_iso.filter(|v| !v.is_empty()) {
        sql.push_str(" AND first_ts <= ?");
        params.push(SqlValue::Text(until.to_owned()));
    }
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(rusqlite::params_from_iter(params.iter()), |row| {
            Ok(SessionCompareRow {
                session_id: row.get(0)?,
                provider: row.get(1)?,
                primary_model: row.get(2)?,
                first_ts: row.get(3)?,
                message_count: row.get(4)?,
                cost_usd: row.get(5)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// One cache-overhead candidate, already through the ratio test.
#[derive(Debug, Clone)]
pub struct CacheOverheadRow {
    /// `session_id` — the detector re-keys this as `session_fk`.
    pub session_id: Option<String>,
    /// `cache_create` from the mart.
    pub cache_create_tokens: i64,
    /// `input_tokens` from the mart.
    pub input_tokens: i64,
    /// `round(cache / (input + cache), 3)`.
    pub ratio: f64,
}

/// `session_mart_cache_overhead` — the ratio test runs in Python, not in SQL.
///
/// Reproduced host-side for the same reason: `cache / (inp + cache)` in SQLite
/// would be integer division on integer columns, and the `round(…, 3)` that
/// reaches the payload is CPython's.
///
/// # Errors
/// Any SQLite error other than a missing `session_mart`, which is `[]`.
pub fn session_mart_cache_overhead(
    conn: &Connection,
    since_iso: Option<&str>,
    until_iso: Option<&str>,
    ratio_threshold: f64,
) -> rusqlite::Result<Vec<CacheOverheadRow>> {
    if !table_exists(conn, "session_mart")? {
        return Ok(Vec::new());
    }
    let mut sql = "SELECT session_id, input_tokens AS inp, cache_create AS cache_create \
                   FROM session_mart WHERE 1=1"
        .to_owned();
    let mut params: Vec<SqlValue> = Vec::new();
    if let Some(since) = since_iso.filter(|v| !v.is_empty()) {
        sql.push_str(" AND first_ts >= ?");
        params.push(SqlValue::Text(since.to_owned()));
    }
    if let Some(until) = until_iso.filter(|v| !v.is_empty()) {
        sql.push_str(" AND first_ts <= ?");
        params.push(SqlValue::Text(until.to_owned()));
    }
    let mut stmt = conn.prepare(&sql)?;
    let raw = stmt
        .query_map(rusqlite::params_from_iter(params.iter()), |row| {
            Ok((
                row.get::<_, Option<String>>(0)?,
                row.get::<_, Option<i64>>(1)?.unwrap_or(0),
                row.get::<_, Option<i64>>(2)?.unwrap_or(0),
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let mut bad = Vec::new();
    for (session_id, inp, cache) in raw {
        // `if inp == 0 or cache == 0: continue` — a session with no cache
        // writes is not thrash, and one with no fresh input is not measurable.
        if inp == 0 || cache == 0 {
            continue;
        }
        let total_input = inp + cache;
        if total_input == 0 {
            continue;
        }
        #[allow(
            clippy::cast_precision_loss,
            reason = "token counts are far below 2^53; Python does the same float divide"
        )]
        let ratio = cache as f64 / total_input as f64;
        if ratio > ratio_threshold {
            bad.push(CacheOverheadRow {
                session_id,
                cache_create_tokens: cache,
                input_tokens: inp,
                ratio: crate::services::optimize::round_half_even(ratio, 3),
            });
        }
    }
    Ok(bad)
}

// ── tool_mart ────────────────────────────────────────────────────────────────

/// The two columns `tool_call_count_in_window` may sum — `_TOOL_COUNT_COLUMNS`.
///
/// A closed enum rather than a string plus a runtime whitelist: Python raises
/// `ValueError` for anything else, and a caller that cannot spell a bad column
/// cannot reach that raise.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolCountColumn {
    /// `event_count` — distinct `(message, tool)` pairs. Python's default.
    EventCount,
    /// `calls_total` — every occurrence. Reads 0 on a pre-v012 `tool_mart`,
    /// which callers using it as a `== 0` short-circuit accept as a
    /// conservative miss (they fall through to the full scan).
    CallsTotal,
}

impl ToolCountColumn {
    const fn as_str(self) -> &'static str {
        match self {
            Self::EventCount => "event_count",
            Self::CallsTotal => "calls_total",
        }
    }
}

/// `tool_call_count_in_window` — `SUM(count_column)` for the named tools.
///
/// The `project_filter` branch does not *append* a join: Python **rebuilds the
/// whole SQL string and the whole parameter list**, which is why the day
/// clauses are re-appended after it. Reproduced structurally so the two
/// parameter orders cannot drift.
///
/// # Errors
/// Any SQLite error other than a missing `tool_mart`, which is `0`.
pub fn tool_call_count_in_window(
    conn: &Connection,
    tool_names: &[&str],
    since_iso: Option<&str>,
    until_iso: Option<&str>,
    project_filter: Option<&[String]>,
    count_column: ToolCountColumn,
) -> rusqlite::Result<i64> {
    if tool_names.is_empty() {
        return Ok(0);
    }
    if !table_exists(conn, "tool_mart")? {
        return Ok(0);
    }
    let col = count_column.as_str();
    let names_ph = placeholders(tool_names.len());
    let day_from = iso_to_day(since_iso);
    let day_to = iso_to_day(until_iso);

    // `if project_filter:` — `None` and `[]` both fall to the unjoined form,
    // and so does a filter whose every entry was empty (`if slugs:` inside).
    let slugs = norm_slugs(project_filter);
    let use_join = !slugs.is_empty();

    let (mut sql, mut params): (String, Vec<SqlValue>) = if use_join {
        (
            format!(
                "SELECT COALESCE(SUM(t.{col}), 0) AS c FROM tool_mart t \
                 JOIN projects p ON p.id = t.project_id \
                 WHERE t.tool_name IN ({names_ph}) AND p.slug IN ({})",
                placeholders(slugs.len())
            ),
            tool_names
                .iter()
                .map(|n| SqlValue::Text((*n).to_owned()))
                .chain(slugs.iter().map(|s| SqlValue::Text(s.clone())))
                .collect(),
        )
    } else {
        (
            format!(
                "SELECT COALESCE(SUM({col}), 0) AS c FROM tool_mart \
                 WHERE tool_name IN ({names_ph})"
            ),
            tool_names
                .iter()
                .map(|n| SqlValue::Text((*n).to_owned()))
                .collect(),
        )
    };
    let day_col = if use_join { "t.day" } else { "day" };
    if let Some(day_from) = day_from {
        let _ = write!(sql, " AND {day_col} >= ?");
        params.push(SqlValue::Text(day_from));
    }
    if let Some(day_to) = day_to {
        let _ = write!(sql, " AND {day_col} <= ?");
        params.push(SqlValue::Text(day_to));
    }

    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query(rusqlite::params_from_iter(params.iter()))?;
    match rows.next()? {
        Some(row) => Ok(row.get::<_, Option<i64>>(0)?.unwrap_or(0)),
        None => Ok(0),
    }
}

/// `tool_mart_distinct_tool_names_in_window` — `SELECT DISTINCT tool_name`.
///
/// `name_prefix` is bound as a `LIKE` pattern with the caller's literal plus
/// `%`. `_detect_unused_mcp_servers` passes `"mcp__"`; the underscore is a
/// single-character `LIKE` wildcard, so the pattern is *looser* than it looks
/// (`mcpXX…` matches `mcp__%`). Harmless — the regex that follows re-checks the
/// literal prefix — and inherited rather than tightened with an `ESCAPE`.
///
/// # Errors
/// Any SQLite error other than a missing `tool_mart`, which is `[]`.
pub fn tool_mart_distinct_tool_names_in_window(
    conn: &Connection,
    since_iso: Option<&str>,
    until_iso: Option<&str>,
    name_prefix: Option<&str>,
) -> rusqlite::Result<Vec<String>> {
    if !table_exists(conn, "tool_mart")? {
        return Ok(Vec::new());
    }
    let mut sql = "SELECT DISTINCT tool_name FROM tool_mart WHERE 1 = 1".to_owned();
    let mut params: Vec<SqlValue> = Vec::new();
    if let Some(prefix) = name_prefix.filter(|v| !v.is_empty()) {
        sql.push_str(" AND tool_name LIKE ?");
        params.push(SqlValue::Text(format!("{prefix}%")));
    }
    push_day_window(&mut sql, &mut params, since_iso, until_iso, "day");
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(rusqlite::params_from_iter(params.iter()), |row| {
            row.get::<_, Option<String>>(0)
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    // `if r[0]` — NULL and the empty string are both dropped.
    Ok(rows
        .into_iter()
        .flatten()
        .filter(|s| !s.is_empty())
        .collect())
}

// ── message_tool_mart ────────────────────────────────────────────────────────

/// One `(session, file)` pair that met the junk-read repeat threshold.
#[derive(Debug, Clone)]
pub struct JunkReadRow {
    /// `session_id`.
    pub session_id: Option<String>,
    /// `file_path`.
    pub file_path: Option<String>,
    /// `COUNT(*)`.
    pub reads: i64,
}

/// `message_tool_junk_reads` — one indexed `GROUP BY … HAVING COUNT(*) >= ?`.
///
/// No `ORDER BY`: the row order is SQLite's, and the caller
/// (`_junk_reads_from_mart`) folds into an insertion-ordered map, so the
/// per-session order in `details.sessions` is the order SQLite emitted the
/// groups in. Inherited, not stabilised.
///
/// # Errors
/// Any SQLite error other than a missing `message_tool_mart`, which is `[]`.
pub fn message_tool_junk_reads(
    conn: &Connection,
    repeat_threshold: i64,
    since_iso: Option<&str>,
    until_iso: Option<&str>,
    project_slugs: Option<&[String]>,
) -> rusqlite::Result<Vec<JunkReadRow>> {
    if !table_exists(conn, "message_tool_mart")? {
        return Ok(Vec::new());
    }
    let slugs = norm_slugs(project_slugs);
    let join = if slugs.is_empty() {
        ""
    } else {
        " JOIN projects p ON p.id = mt.project_id"
    };
    let mut sql = format!(
        "SELECT mt.session_id AS session_id, mt.file_path AS file_path, COUNT(*) AS reads \
         FROM message_tool_mart mt{join} \
         WHERE mt.tool_name = 'Read' AND mt.file_path IS NOT NULL"
    );
    let mut params: Vec<SqlValue> = Vec::new();
    if !slugs.is_empty() {
        let _ = write!(sql, " AND p.slug IN ({})", placeholders(slugs.len()));
        params.extend(slugs.iter().map(|s| SqlValue::Text(s.clone())));
    }
    push_day_window(&mut sql, &mut params, since_iso, until_iso, "mt.day");
    sql.push_str(" GROUP BY mt.session_id, mt.file_path HAVING COUNT(*) >= ?");
    params.push(SqlValue::Integer(repeat_threshold));

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(rusqlite::params_from_iter(params.iter()), |row| {
            Ok(JunkReadRow {
                session_id: row.get(0)?,
                file_path: row.get(1)?,
                reads: row.get::<_, Option<i64>>(2)?.unwrap_or(0),
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// One session's `(Read, write-family)` call counts.
#[derive(Debug, Clone)]
pub struct ReadEditRow {
    /// `session_id`.
    pub session_id: Option<String>,
    /// `SUM(CASE WHEN tool_name = 'Read' …)`.
    pub reads: i64,
    /// `SUM(CASE WHEN tool_name IN ('Edit','Write','MultiEdit','NotebookEdit') …)`.
    pub edits: i64,
}

/// `message_tool_read_edit_per_session`.
///
/// The write family is exactly `Edit`, `Write`, `MultiEdit`, `NotebookEdit` —
/// the same four the raw-scan fallback checks, so the two sources agree on what
/// "never wrote code" means.
///
/// # Errors
/// Any SQLite error other than a missing `message_tool_mart`, which is `[]`.
pub fn message_tool_read_edit_per_session(
    conn: &Connection,
    since_iso: Option<&str>,
    until_iso: Option<&str>,
    project_slugs: Option<&[String]>,
) -> rusqlite::Result<Vec<ReadEditRow>> {
    if !table_exists(conn, "message_tool_mart")? {
        return Ok(Vec::new());
    }
    let slugs = norm_slugs(project_slugs);
    let join = if slugs.is_empty() {
        ""
    } else {
        " JOIN projects p ON p.id = mt.project_id"
    };
    let mut sql = format!(
        "SELECT mt.session_id AS session_id, \
                SUM(CASE WHEN mt.tool_name = 'Read' THEN 1 ELSE 0 END) AS reads, \
                SUM(CASE WHEN mt.tool_name IN \
                    ('Edit', 'Write', 'MultiEdit', 'NotebookEdit') THEN 1 ELSE 0 END) AS edits \
         FROM message_tool_mart mt{join} WHERE 1 = 1"
    );
    let mut params: Vec<SqlValue> = Vec::new();
    if !slugs.is_empty() {
        let _ = write!(sql, " AND p.slug IN ({})", placeholders(slugs.len()));
        params.extend(slugs.iter().map(|s| SqlValue::Text(s.clone())));
    }
    push_day_window(&mut sql, &mut params, since_iso, until_iso, "mt.day");
    sql.push_str(" GROUP BY mt.session_id");

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(rusqlite::params_from_iter(params.iter()), |row| {
            Ok(ReadEditRow {
                session_id: row.get(0)?,
                reads: row.get::<_, Option<i64>>(1)?.unwrap_or(0),
                edits: row.get::<_, Option<i64>>(2)?.unwrap_or(0),
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// One oversized tool result.
#[derive(Debug, Clone)]
pub struct OversizedRow {
    /// `session_id`.
    pub session_id: Option<String>,
    /// `message_id` — surfaced under the key `seq` for fallback parity.
    pub message_id: i64,
    /// `byte_count`.
    pub byte_count: i64,
}

/// `message_tool_oversized` — rows for `tool_name` over `threshold_bytes`.
///
/// The comparison is **strictly** `>`, while the raw-scan fallback skips on
/// `size < threshold` (i.e. keeps `==`). One byte of daylight between the two
/// sources at exactly 50 000 bytes. Inherited.
///
/// # Errors
/// Any SQLite error other than a missing `message_tool_mart`, which is `[]`.
pub fn message_tool_oversized(
    conn: &Connection,
    tool_name: &str,
    threshold_bytes: i64,
    since_iso: Option<&str>,
    until_iso: Option<&str>,
    project_slugs: Option<&[String]>,
) -> rusqlite::Result<Vec<OversizedRow>> {
    if !table_exists(conn, "message_tool_mart")? {
        return Ok(Vec::new());
    }
    let slugs = norm_slugs(project_slugs);
    let join = if slugs.is_empty() {
        ""
    } else {
        " JOIN projects p ON p.id = mt.project_id"
    };
    let mut sql = format!(
        "SELECT mt.session_id AS session_id, mt.message_id AS message_id, \
                mt.byte_count AS byte_count \
         FROM message_tool_mart mt{join} \
         WHERE mt.tool_name = ? AND mt.byte_count IS NOT NULL AND mt.byte_count > ?"
    );
    let mut params: Vec<SqlValue> = vec![
        SqlValue::Text(tool_name.to_owned()),
        SqlValue::Integer(threshold_bytes),
    ];
    if !slugs.is_empty() {
        let _ = write!(sql, " AND p.slug IN ({})", placeholders(slugs.len()));
        params.extend(slugs.iter().map(|s| SqlValue::Text(s.clone())));
    }
    push_day_window(&mut sql, &mut params, since_iso, until_iso, "mt.day");
    sql.push_str(" ORDER BY mt.byte_count DESC");

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(rusqlite::params_from_iter(params.iter()), |row| {
            Ok(OversizedRow {
                session_id: row.get(0)?,
                message_id: row.get::<_, Option<i64>>(1)?.unwrap_or(0),
                byte_count: row.get::<_, Option<i64>>(2)?.unwrap_or(0),
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// `message_tool_invoked_agents` — the distinct `subagent_type` set.
///
/// The mart stores each `Task` call's `subagent_type` in the `file_path`
/// column. Yes, really: one column, two meanings, keyed off `tool_name`.
///
/// # Errors
/// Any SQLite error other than a missing `message_tool_mart`, which is empty.
pub fn message_tool_invoked_agents(
    conn: &Connection,
    since_iso: Option<&str>,
    until_iso: Option<&str>,
) -> rusqlite::Result<HashSet<String>> {
    if !table_exists(conn, "message_tool_mart")? {
        return Ok(HashSet::new());
    }
    let mut sql = "SELECT DISTINCT file_path FROM message_tool_mart \
                   WHERE tool_name = 'Task' AND file_path IS NOT NULL"
        .to_owned();
    let mut params: Vec<SqlValue> = Vec::new();
    push_day_window(&mut sql, &mut params, since_iso, until_iso, "day");
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(rusqlite::params_from_iter(params.iter()), |row| {
            row.get::<_, Option<String>>(0)
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    // `if r["file_path"]` — the empty string is dropped as well as NULL.
    Ok(rows
        .into_iter()
        .flatten()
        .filter(|s| !s.is_empty())
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> Connection {
        let conn = Connection::open_in_memory().expect("in-memory");
        conn.execute_batch(
            "CREATE TABLE projects (id INTEGER PRIMARY KEY, slug TEXT);
             CREATE TABLE message_tool_mart (
                 project_id INTEGER, session_id TEXT, message_id INTEGER,
                 tool_name TEXT, file_path TEXT, byte_count INTEGER, day TEXT);
             CREATE TABLE tool_mart (
                 project_id INTEGER, tool_name TEXT, day TEXT,
                 event_count INTEGER, calls_total INTEGER);
             CREATE TABLE session_mart (
                 session_id TEXT, project_id INTEGER, provider TEXT,
                 primary_model TEXT, first_ts TEXT, message_count INTEGER,
                 input_tokens INTEGER, cache_create INTEGER, cost_usd REAL);
             CREATE TABLE daily_mart (day TEXT, cost_usd REAL);
             INSERT INTO projects VALUES (1, 'alpha'), (2, 'beta');",
        )
        .expect("schema");
        conn
    }

    #[test]
    fn an_absent_table_is_false_and_empty_never_an_error() {
        let conn = Connection::open_in_memory().expect("in-memory");
        assert!(!table_exists(&conn, "tool_mart").expect("guarded"));
        assert!(!mart_has_tool_rows(&conn).expect("guarded"));
        assert!(!mart_has_session_rows(&conn).expect("guarded"));
        assert!(!mart_has_message_tool_rows(&conn).expect("guarded"));
        assert!(daily_global(&conn, None, None).expect("guarded").is_empty());
        assert_eq!(
            tool_call_count_in_window(
                &conn,
                &["Read"],
                None,
                None,
                None,
                ToolCountColumn::EventCount
            )
            .expect("guarded"),
            0
        );
    }

    #[test]
    fn a_bound_shorter_than_ten_characters_removes_the_clause_entirely() {
        // `_iso_to_day` returns None for anything under ten chars, so a junk
        // bound WIDENS the window instead of erroring or matching nothing.
        assert_eq!(
            iso_to_day(Some("2026-07-31T00:00:00")).as_deref(),
            Some("2026-07-31")
        );
        assert_eq!(iso_to_day(Some("2026-07-3")), None);
        assert_eq!(iso_to_day(Some("")), None);
        assert_eq!(iso_to_day(None), None);
    }

    #[test]
    fn an_empty_slug_list_means_all_projects_not_no_projects() {
        let conn = store();
        conn.execute_batch(
            "INSERT INTO tool_mart VALUES (1, 'Read', '2026-07-01', 4, 9),
                                          (2, 'Read', '2026-07-01', 6, 11);",
        )
        .expect("rows");
        let all = tool_call_count_in_window(
            &conn,
            &["Read"],
            None,
            None,
            None,
            ToolCountColumn::EventCount,
        )
        .expect("query");
        let empty_filter = tool_call_count_in_window(
            &conn,
            &["Read"],
            None,
            None,
            Some(&[]),
            ToolCountColumn::EventCount,
        )
        .expect("query");
        assert_eq!(all, 10);
        assert_eq!(empty_filter, 10, "`if project_filter:` is falsy on []");
        // …and a real slug narrows.
        let one = tool_call_count_in_window(
            &conn,
            &["Read"],
            None,
            None,
            Some(&["alpha".to_owned()]),
            ToolCountColumn::EventCount,
        )
        .expect("query");
        assert_eq!(one, 4);
    }

    #[test]
    fn calls_total_and_event_count_are_different_measures() {
        let conn = store();
        conn.execute_batch("INSERT INTO tool_mart VALUES (1, 'Bash', '2026-07-01', 2, 7);")
            .expect("rows");
        assert_eq!(
            tool_call_count_in_window(
                &conn,
                &["Bash"],
                None,
                None,
                None,
                ToolCountColumn::EventCount
            )
            .expect("query"),
            2
        );
        assert_eq!(
            tool_call_count_in_window(
                &conn,
                &["Bash"],
                None,
                None,
                None,
                ToolCountColumn::CallsTotal
            )
            .expect("query"),
            7
        );
    }

    #[test]
    fn the_day_window_is_applied_to_the_joined_alias_too() {
        let conn = store();
        conn.execute_batch(
            "INSERT INTO tool_mart VALUES (1, 'Read', '2026-06-01', 5, 5),
                                          (1, 'Read', '2026-07-05', 3, 3);",
        )
        .expect("rows");
        let joined = tool_call_count_in_window(
            &conn,
            &["Read"],
            Some("2026-07-01T00:00:00+00:00"),
            None,
            Some(&["alpha".to_owned()]),
            ToolCountColumn::EventCount,
        )
        .expect("query");
        assert_eq!(
            joined, 3,
            "t.day, not day — an unqualified column is ambiguous"
        );
    }

    #[test]
    fn junk_reads_applies_the_having_threshold_not_a_post_filter() {
        let conn = store();
        for i in 0..5 {
            conn.execute(
                "INSERT INTO message_tool_mart VALUES (1, 's1', ?, 'Read', '/a.rs', NULL, '2026-07-01')",
                [i],
            )
            .expect("row");
        }
        conn.execute(
            "INSERT INTO message_tool_mart VALUES (1, 's1', 99, 'Read', '/b.rs', NULL, '2026-07-01')",
            [],
        )
        .expect("row");
        let rows = message_tool_junk_reads(&conn, 5, None, None, None).expect("query");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].file_path.as_deref(), Some("/a.rs"));
        assert_eq!(rows[0].reads, 5);
    }

    #[test]
    fn oversized_is_strictly_greater_than_the_threshold() {
        let conn = store();
        conn.execute_batch(
            "INSERT INTO message_tool_mart VALUES
                 (1, 's1', 1, 'Bash', NULL, 50000, '2026-07-01'),
                 (1, 's1', 2, 'Bash', NULL, 50001, '2026-07-01');",
        )
        .expect("rows");
        let rows = message_tool_oversized(&conn, "Bash", 50_000, None, None, None).expect("query");
        // The exact-threshold row is OUT here and IN on the raw-scan fallback.
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].byte_count, 50_001);
    }

    #[test]
    fn the_invoked_agent_set_reads_subagent_type_out_of_file_path() {
        let conn = store();
        conn.execute_batch(
            "INSERT INTO message_tool_mart VALUES
                 (1, 's1', 1, 'Task', 'Explore', NULL, '2026-07-01'),
                 (1, 's1', 2, 'Task', 'Explore', NULL, '2026-07-02'),
                 (1, 's1', 3, 'Task', '', NULL, '2026-07-02'),
                 (1, 's1', 4, 'Read', '/x.rs', NULL, '2026-07-02');",
        )
        .expect("rows");
        let set = message_tool_invoked_agents(&conn, None, None).expect("query");
        assert_eq!(set.len(), 1, "empty string is dropped, Read is not a Task");
        assert!(set.contains("Explore"));
    }

    #[test]
    fn cache_overhead_skips_the_zero_legs_and_rounds_to_three_places() {
        let conn = store();
        conn.execute_batch(
            "INSERT INTO session_mart VALUES
                 ('a', 1, 'claude', 'm', '2026-07-01T00:00:00+00:00', 3, 100, 900, 1.0),
                 ('b', 1, 'claude', 'm', '2026-07-01T00:00:00+00:00', 3, 0,   900, 1.0),
                 ('c', 1, 'claude', 'm', '2026-07-01T00:00:00+00:00', 3, 100, 0,   1.0),
                 ('d', 1, 'claude', 'm', '2026-07-01T00:00:00+00:00', 3, 900, 100, 1.0);",
        )
        .expect("rows");
        let bad = session_mart_cache_overhead(&conn, None, None, 0.5).expect("query");
        assert_eq!(bad.len(), 1, "b and c are skipped, d is under the ratio");
        assert_eq!(bad[0].session_id.as_deref(), Some("a"));
        assert!((bad[0].ratio - 0.9).abs() < 1e-12);
    }

    #[test]
    fn session_rows_window_on_first_ts_the_session_start() {
        let conn = store();
        conn.execute_batch(
            "INSERT INTO session_mart VALUES
                 ('early', 1, 'claude', 'm', '2026-06-30T23:59:59+00:00', 3, 1, 1, 2.5),
                 ('late',  1, 'claude', 'm', '2026-07-01T00:00:00+00:00', 4, 1, 1, 3.5);",
        )
        .expect("rows");
        let rows = session_mart_rows_for_compare(&conn, Some("2026-07-01T00:00:00+00:00"), None)
            .expect("query");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].session_id.as_deref(), Some("late"));
        assert_eq!(rows[0].message_count, Some(4));
    }
}
