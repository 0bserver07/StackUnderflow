//! `reports/aggregate.py` — the cross-project rollup behind `/api/plan`,
//! `/api/export` and `stackunderflow month`.
//!
//! | Item | Python | Rust |
//! |---|---|---|
//! | `build_report` | the whole rollup | [`build_report`] |
//! | `_has_usage_events` | the mart gate | [`has_usage_events`] |
//! | `_per_slug_from_usage_events` | post-backfill path | private |
//! | `_per_slug_from_messages` | pre-backfill fallback | private |
//!
//! `usage_events.cost_usd` is the source of truth: it was normalised once on
//! ingest by the live pricer, it already carries the `speed='fast'` priority
//! multiplier, and it already encodes the 1/N attribution contract the marts
//! define. This module just sums it. The `messages`-based path exists only for
//! a store where the ETL backfill has not run — a fresh install — and
//! re-derives cost from `(input_tokens, output_tokens, model, speed)`, which
//! mis-prices anything whose model alias canonicalised differently.
//!
//! # What is load-bearing
//!
//! * **The gate is "exists AND has at least one row."** An empty
//!   `usage_events` is not "zero spend", it is "not backfilled yet", and the
//!   two paths give materially different numbers. A store where the gate flips
//!   is exactly where an unported branch would hide, so both are ported.
//! * **The two paths do not agree on the upper bound.**
//!   `_per_slug_from_usage_events` filters `ts <= :until` (inclusive);
//!   `_per_slug_from_messages` filters `timestamp < :until` (half-open),
//!   through both `cross_project_daily_totals` and its own session query. That
//!   is inconsistent in the reference and it is reproduced, not reconciled
//!   (DIV-090).
//! * **`if since:` is truthiness.** An empty-string bound is falsy, so the
//!   clause is omitted entirely rather than compared against `""` — which as a
//!   lexicographic lower bound would have matched everything anyway, but as an
//!   *upper* bound would have matched nothing.
//! * **Dict insertion order is the tie-break.** `per_project.sort(key=cost,
//!   reverse=True)` is a stable descending sort, so two projects with identical
//!   cost come out in the order the SQL produced them. The per-slug map is
//!   therefore an insertion-ordered vector here, not a `HashMap`.
//! * **`total_cost` starts at the FLOAT `0.0` and accumulates with `+=`.** Not
//!   `sum()`, so the accumulation is plain (law 3), and the seed being a float
//!   means an empty report renders `0.0` rather than `0`.
//! * **Pricing is injected.** The fallback path takes a `&PricingEngine` built
//!   by `crate::pricing::engine`, never `default_engine()` — the
//!   manifest-vs-primed-`price_book` seam is a 2% silent mispricing that no
//!   test on an unprimed store can catch (law 2 / DIV-056).

use std::collections::HashMap;

use rusqlite::Connection;
use serde_json::{Map, Value};
use stax_etl::pricing::RawTokens;
use stax_etl::pricing::costs::PricingEngine;

use crate::scope::Scope;

/// One row of `by_project`. Key order is the dict-literal order.
#[derive(Debug, Clone, PartialEq)]
pub struct ProjectRow {
    /// `name` — the project slug.
    pub name: String,
    /// `cost` — USD.
    pub cost: f64,
    /// `messages` — event count in the window.
    pub messages: i64,
    /// `sessions` — distinct sessions in the window.
    pub sessions: i64,
}

/// What `build_report` returns. Key order is the dict-literal order.
#[derive(Debug, Clone, PartialEq)]
pub struct Report {
    /// `scope_label` — echoed straight from the scope.
    pub scope_label: String,
    /// `total_cost` — always a float, even at zero.
    pub total_cost: f64,
    /// `total_messages`.
    pub total_messages: i64,
    /// `total_sessions`.
    pub total_sessions: i64,
    /// `by_project` — sorted by cost, descending, stably.
    pub by_project: Vec<ProjectRow>,
}

impl Report {
    /// The report as the JSON object Python builds, in the dict-literal order.
    ///
    /// `/api/plan` reads only `total_cost`, but `reports/export.py` renders the
    /// whole thing, so the key order lives here rather than being re-derived by
    /// each caller.
    #[must_use]
    pub fn to_value(&self) -> Value {
        let mut out = Map::new();
        out.insert(
            "scope_label".to_owned(),
            Value::String(self.scope_label.clone()),
        );
        out.insert("total_cost".to_owned(), Value::from(self.total_cost));
        out.insert(
            "total_messages".to_owned(),
            Value::from(self.total_messages),
        );
        out.insert(
            "total_sessions".to_owned(),
            Value::from(self.total_sessions),
        );
        out.insert(
            "by_project".to_owned(),
            Value::Array(
                self.by_project
                    .iter()
                    .map(|row| {
                        let mut item = Map::new();
                        item.insert("name".to_owned(), Value::String(row.name.clone()));
                        item.insert("cost".to_owned(), Value::from(row.cost));
                        item.insert("messages".to_owned(), Value::from(row.messages));
                        item.insert("sessions".to_owned(), Value::from(row.sessions));
                        Value::Object(item)
                    })
                    .collect(),
            ),
        );
        Value::Object(out)
    }
}

/// One per-slug accumulator — the inner dict of `per_slug`.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct Entry {
    cost: f64,
    messages: i64,
    sessions: i64,
}

/// An insertion-ordered `dict[str, Entry]`.
///
/// A plain `HashMap` would lose the ordering the stable sort tie-breaks on; a
/// plain `Vec` would make the fallback path's `setdefault` quadratic over the
/// per-(slug, day, model, speed) rollup. So: both.
#[derive(Debug, Default)]
struct PerSlug {
    order: Vec<(String, Entry)>,
    index: HashMap<String, usize>,
}

impl PerSlug {
    /// `per_slug.setdefault(slug, {...})` — returns a handle to the entry.
    fn entry(&mut self, slug: &str) -> &mut Entry {
        let position = match self.index.get(slug) {
            Some(position) => *position,
            None => {
                let position = self.order.len();
                self.order.push((slug.to_owned(), Entry::default()));
                self.index.insert(slug.to_owned(), position);
                position
            }
        };
        &mut self.order[position].1
    }

    /// `per_slug.get(slug)`, mutable, WITHOUT inserting.
    fn get_mut(&mut self, slug: &str) -> Option<&mut Entry> {
        let position = *self.index.get(slug)?;
        Some(&mut self.order[position].1)
    }
}

/// `build_report(conn, scope=…, include=…, exclude=…)`.
///
/// `include`/`exclude` are `None` for "no filter" — distinct from `Some(&[])`,
/// which Python spells as an empty list and which filters *everything* out
/// (`include=[]` keeps nothing; `exclude=[]` keeps everything). Both are
/// reproduced by the `is not None` tests below rather than by truthiness,
/// because that is what the Python writes.
///
/// # Errors
/// Any SQLite error from the two (or three) queries.
pub fn build_report(
    conn: &Connection,
    scope: &Scope,
    include: Option<&[String]>,
    exclude: Option<&[String]>,
    engine: &PricingEngine,
) -> rusqlite::Result<Report> {
    let since = truthy(scope.since.as_deref());
    let until = truthy(scope.until.as_deref());

    let per_slug = if has_usage_events(conn)? {
        per_slug_from_usage_events(conn, since, until)?
    } else {
        per_slug_from_messages(conn, since, until, engine)?
    };

    // `{k: v for k, v in per_slug.items() if k in include}` — a comprehension,
    // so the surviving entries keep the map's order, not the filter list's.
    let mut rows = per_slug.order;
    if let Some(include) = include {
        rows.retain(|(slug, _)| include.iter().any(|wanted| wanted == slug));
    }
    if let Some(exclude) = exclude {
        rows.retain(|(slug, _)| !exclude.iter().any(|skipped| skipped == slug));
    }

    let mut per_project: Vec<ProjectRow> = Vec::with_capacity(rows.len());
    // `total_cost = 0.0` — a FLOAT seed, so an empty report still renders
    // `0.0`. `total_messages` / `total_sessions` seed at the int `0`.
    let mut total_cost = 0.0_f64;
    let mut total_messages = 0_i64;
    let mut total_sessions = 0_i64;

    for (slug, data) in rows {
        per_project.push(ProjectRow {
            name: slug,
            cost: data.cost,
            messages: data.messages,
            sessions: data.sessions,
        });
        // Plain `+=`, not a compensated sum — Python writes the loop out.
        total_cost += data.cost;
        total_messages += data.messages;
        total_sessions += data.sessions;
    }

    // `sort(key=…, reverse=True)` is a STABLE descending sort: equal costs keep
    // the map's insertion order. Comparing `b` to `a` preserves that; sorting
    // ascending and reversing would not.
    per_project.sort_by(|a, b| {
        b.cost
            .partial_cmp(&a.cost)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    Ok(Report {
        scope_label: scope.label.clone(),
        total_cost,
        total_messages,
        total_sessions,
        by_project: per_project,
    })
}

/// `if since:` — an empty bound is falsy and drops the clause entirely.
fn truthy(bound: Option<&str>) -> Option<&str> {
    bound.filter(|text| !text.is_empty())
}

/// `_has_usage_events(conn)` — the table exists **and** has at least one row.
///
/// The `type='table'` test is literal: a `usage_events` *view* would report
/// absent here and send the caller down the legacy path, which is what the
/// reference does.
///
/// # Errors
/// Any SQLite error other than the empty result both probes tolerate.
pub fn has_usage_events(conn: &Connection) -> rusqlite::Result<bool> {
    let exists: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name='usage_events'",
            [],
            |row| row.get(0),
        )
        .optional_row()?;
    if exists.is_none() {
        return Ok(false);
    }
    let row: Option<i64> = conn
        .query_row("SELECT 1 FROM usage_events LIMIT 1", [], |row| row.get(0))
        .optional_row()?;
    Ok(row.is_some())
}

/// `_per_slug_from_usage_events` — two passes over the mart.
///
/// The SQL is transcribed clause for clause, including the `WHERE 1=1` the
/// optional bounds are appended to and the second query's `COUNT(DISTINCT
/// session_id)` (a distinct-inside-group-by rather than a round trip to
/// `sessions`).
fn per_slug_from_usage_events(
    conn: &Connection,
    since: Option<&str>,
    until: Option<&str>,
) -> rusqlite::Result<PerSlug> {
    let mut sql = String::from(
        "SELECT projects.slug AS slug, \
                COALESCE(SUM(usage_events.cost_usd), 0.0) AS cost, \
                COUNT(*) AS messages \
         FROM usage_events \
         JOIN projects ON projects.id = usage_events.project_id \
         WHERE 1=1 ",
    );
    let mut params: Vec<&str> = Vec::new();
    if let Some(since) = since {
        sql.push_str("AND usage_events.ts >= ? ");
        params.push(since);
    }
    // NOTE: `<=`, inclusive — the messages path below uses `<`. See DIV-090.
    if let Some(until) = until {
        sql.push_str("AND usage_events.ts <= ? ");
        params.push(until);
    }
    sql.push_str("GROUP BY projects.slug");

    let mut per_slug = PerSlug::default();
    {
        let mut stmt = conn.prepare(&sql)?;
        let mut rows = stmt.query(rusqlite::params_from_iter(params.iter()))?;
        while let Some(row) = rows.next()? {
            let slug: String = row.get(0)?;
            // `float(row["cost"] or 0.0)` — the COALESCE already rules NULL out,
            // and the `or` additionally maps a genuine 0.0 to 0.0.
            let cost: Option<f64> = row.get(1)?;
            let messages: Option<i64> = row.get(2)?;
            let entry = per_slug.entry(&slug);
            entry.cost = cost.unwrap_or(0.0);
            entry.messages = messages.unwrap_or(0);
            entry.sessions = 0;
        }
    }

    let mut session_sql = String::from(
        "SELECT projects.slug AS slug, \
                COUNT(DISTINCT usage_events.session_id) AS cnt \
         FROM usage_events \
         JOIN projects ON projects.id = usage_events.project_id \
         WHERE 1=1 ",
    );
    let mut s_params: Vec<&str> = Vec::new();
    if let Some(since) = since {
        session_sql.push_str("AND usage_events.ts >= ? ");
        s_params.push(since);
    }
    if let Some(until) = until {
        session_sql.push_str("AND usage_events.ts <= ? ");
        s_params.push(until);
    }
    session_sql.push_str("GROUP BY projects.slug");

    let mut stmt = conn.prepare(&session_sql)?;
    let mut rows = stmt.query(rusqlite::params_from_iter(s_params.iter()))?;
    while let Some(row) = rows.next()? {
        let slug: String = row.get(0)?;
        let count: Option<i64> = row.get(1)?;
        // `if slug in per_slug:` — a slug the first pass did not see is dropped,
        // never inserted. The two queries share a WHERE, so this cannot fire,
        // but the guard is in the Python and the shape is kept.
        if let Some(entry) = per_slug.get_mut(&slug) {
            entry.sessions = count.unwrap_or(0);
        }
    }

    Ok(per_slug)
}

/// `_per_slug_from_messages` — the pre-backfill fallback.
///
/// Recomputes cost from stored token counts through the injected pricer. Two
/// details a paraphrase would drop:
///
/// * `if model:` is truthiness over `COALESCE(messages.model, '')`, so a row
///   with no model contributes its message COUNT but no cost at all.
/// * the token dict handed to `compute_cost` has exactly two keys, `input` and
///   `output`. The cache buckets are absent, not zero — which for the Anthropic
///   pricer is the same arithmetic, and for a provider whose `normalize_tokens`
///   branches on key presence would not be.
fn per_slug_from_messages(
    conn: &Connection,
    since: Option<&str>,
    until: Option<&str>,
    engine: &PricingEngine,
) -> rusqlite::Result<PerSlug> {
    // `queries.cross_project_daily_totals(conn, since=…, until=…)`.
    let mut sql = String::from(
        "SELECT projects.slug AS slug, \
                substr(messages.timestamp, 1, 10) AS day, \
                COALESCE(messages.model, '') AS model, \
                SUM(messages.input_tokens) AS input_tokens, \
                SUM(messages.output_tokens) AS output_tokens, \
                COUNT(*) AS messages, \
                COALESCE(messages.speed, 'standard') AS speed \
         FROM messages \
         JOIN sessions ON sessions.id = messages.session_fk \
         JOIN projects ON projects.id = sessions.project_id \
         WHERE 1=1 ",
    );
    let mut params: Vec<&str> = Vec::new();
    if let Some(since) = since {
        sql.push_str("AND messages.timestamp >= ? ");
        params.push(since);
    }
    // Half-open here, inclusive in the mart path. DIV-090.
    if let Some(until) = until {
        sql.push_str("AND messages.timestamp < ? ");
        params.push(until);
    }
    sql.push_str("GROUP BY slug, day, model, speed ORDER BY day");

    // The session count is resolved FIRST in Python, before the cost loop.
    let mut session_sql = String::from(
        "SELECT projects.slug, COUNT(DISTINCT sessions.id) AS cnt \
         FROM sessions \
         JOIN projects ON projects.id = sessions.project_id \
         JOIN messages ON messages.session_fk = sessions.id \
         WHERE 1=1 ",
    );
    let mut s_params: Vec<&str> = Vec::new();
    if let Some(since) = since {
        session_sql.push_str("AND messages.timestamp >= ? ");
        s_params.push(since);
    }
    if let Some(until) = until {
        session_sql.push_str("AND messages.timestamp < ? ");
        s_params.push(until);
    }
    session_sql.push_str("GROUP BY projects.slug");

    let mut session_counts: HashMap<String, i64> = HashMap::new();
    {
        let mut stmt = conn.prepare(&session_sql)?;
        let mut rows = stmt.query(rusqlite::params_from_iter(s_params.iter()))?;
        while let Some(row) = rows.next()? {
            let slug: String = row.get(0)?;
            let count: Option<i64> = row.get(1)?;
            // `dict(rows)` — a duplicate key would keep the LAST row's value.
            session_counts.insert(slug, count.unwrap_or(0));
        }
    }

    let mut per_slug = PerSlug::default();
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query(rusqlite::params_from_iter(params.iter()))?;
    while let Some(row) = rows.next()? {
        let slug: String = row.get(0)?;
        let model: String = row.get(2)?;
        let input_tokens: Option<i64> = row.get(3)?;
        let output_tokens: Option<i64> = row.get(4)?;
        let msg_count: i64 = row.get(5)?;
        let speed: String = row.get(6)?;

        let entry = per_slug.entry(&slug);
        entry.messages += msg_count;
        // `if model:` — the COALESCE'd empty string is falsy.
        if !model.is_empty() {
            let mut tokens = RawTokens::empty();
            // `input_tokens or 0` — NULL and 0 both become 0.
            tokens.set("input", input_tokens.unwrap_or(0));
            tokens.set("output", output_tokens.unwrap_or(0));
            // `compute_cost(tokens, model, speed=speed)` — provider defaults to
            // "anthropic" and `at_ts` is not passed.
            entry.cost += engine
                .compute_cost(&tokens, &model, "anthropic", &speed, None)
                .total_cost;
        }
    }

    // `for slug in per_slug: per_slug[slug]["sessions"] = session_counts.get(slug, 0)`
    for (slug, entry) in &mut per_slug.order {
        entry.sessions = session_counts.get(slug).copied().unwrap_or(0);
    }

    Ok(per_slug)
}

/// `query_row` with "no rows" folded into `None` instead of an error.
trait OptionalRow<T> {
    fn optional_row(self) -> rusqlite::Result<Option<T>>;
}

impl<T> OptionalRow<T> for rusqlite::Result<T> {
    fn optional_row(self) -> rusqlite::Result<Option<T>> {
        match self {
            Ok(value) => Ok(Some(value)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(err) => Err(err),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two tables both paths need, plus the mart.
    fn fixture(with_mart: bool) -> Connection {
        let conn = Connection::open_in_memory().expect("in-memory");
        conn.execute_batch(
            "CREATE TABLE projects (id INTEGER PRIMARY KEY, slug TEXT NOT NULL);
             CREATE TABLE sessions (id INTEGER PRIMARY KEY, project_id INTEGER NOT NULL);
             CREATE TABLE messages (
                 id INTEGER PRIMARY KEY, session_fk INTEGER NOT NULL,
                 timestamp TEXT, model TEXT, speed TEXT,
                 input_tokens INTEGER, output_tokens INTEGER);
             INSERT INTO projects (id, slug) VALUES (1, 'alpha'), (2, 'beta');
             INSERT INTO sessions (id, project_id) VALUES (10, 1), (11, 1), (20, 2);",
        )
        .expect("schema");
        if with_mart {
            conn.execute_batch(
                "CREATE TABLE usage_events (
                     id INTEGER PRIMARY KEY, project_id INTEGER NOT NULL,
                     session_id TEXT, ts TEXT, cost_usd REAL);",
            )
            .expect("mart schema");
        }
        conn
    }

    fn engine() -> PricingEngine {
        let manifest = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../../stackunderflow/data/models.toml");
        PricingEngine::from_manifest_path(&manifest).expect("the shipped rate card")
    }

    fn all_time() -> Scope {
        Scope::new(None, None, "all time")
    }

    #[test]
    fn an_absent_or_empty_mart_sends_the_caller_down_the_legacy_path() {
        // No table at all.
        let conn = fixture(false);
        assert!(!has_usage_events(&conn).expect("probe"));

        // Table present but empty — "not backfilled yet", not "zero spend".
        let conn = fixture(true);
        assert!(!has_usage_events(&conn).expect("probe"));

        conn.execute(
            "INSERT INTO usage_events (project_id, session_id, ts, cost_usd) \
             VALUES (1, 's', '2026-07-01T00:00:00+00:00', 1.0)",
            [],
        )
        .expect("insert");
        assert!(has_usage_events(&conn).expect("probe"));
    }

    #[test]
    fn the_mart_path_sums_cost_and_counts_distinct_sessions() {
        let conn = fixture(true);
        conn.execute_batch(
            "INSERT INTO usage_events (project_id, session_id, ts, cost_usd) VALUES
                 (1, 'a', '2026-07-01T00:00:00+00:00', 1.5),
                 (1, 'a', '2026-07-02T00:00:00+00:00', 2.5),
                 (1, 'b', '2026-07-03T00:00:00+00:00', 1.0),
                 (2, 'c', '2026-07-04T00:00:00+00:00', 9.0);",
        )
        .expect("rows");

        let report = build_report(&conn, &all_time(), None, None, &engine()).expect("report");
        assert_eq!(report.total_messages, 4);
        assert_eq!(report.total_sessions, 3);
        assert!((report.total_cost - 14.0).abs() < 1e-12);
        // Sorted by cost, descending: beta ($9) before alpha ($5).
        assert_eq!(
            report
                .by_project
                .iter()
                .map(|r| r.name.as_str())
                .collect::<Vec<_>>(),
            vec!["beta", "alpha"]
        );
        assert_eq!(report.by_project[1].sessions, 2);
    }

    #[test]
    fn the_marts_upper_bound_is_inclusive_and_the_legacy_paths_is_not() {
        // The reference's own inconsistency (DIV-090): an event stamped EXACTLY
        // at `until` is inside the mart window and outside the messages one.
        let conn = fixture(true);
        conn.execute_batch(
            "INSERT INTO usage_events (project_id, session_id, ts, cost_usd) VALUES
                 (1, 'a', '2026-07-31T00:00:00', 5.0);",
        )
        .expect("rows");
        let scope = Scope::new(
            Some("2026-07-01T00:00:00".to_owned()),
            Some("2026-07-31T00:00:00".to_owned()),
            "plan-period",
        );
        let report = build_report(&conn, &scope, None, None, &engine()).expect("report");
        assert_eq!(report.total_messages, 1);

        // The same instant through the legacy path is excluded by `<`.
        let conn = fixture(false);
        conn.execute_batch(
            "INSERT INTO messages (session_fk, timestamp, model, input_tokens, output_tokens)
             VALUES (10, '2026-07-31T00:00:00', 'claude-sonnet-4-5', 100, 100);",
        )
        .expect("rows");
        let report = build_report(&conn, &scope, None, None, &engine()).expect("report");
        assert_eq!(report.total_messages, 0);
    }

    #[test]
    fn an_empty_report_still_renders_total_cost_as_a_float() {
        // `total_cost = 0.0` seeds a FLOAT. Seeding an int would render `0` and
        // be a one-byte divergence on every quiet window.
        let conn = fixture(true);
        conn.execute(
            "INSERT INTO usage_events (project_id, session_id, ts, cost_usd) \
             VALUES (1, 'a', '1999-01-01T00:00:00', 1.0)",
            [],
        )
        .expect("insert");
        let scope = Scope::new(
            Some("2026-07-01T00:00:00".to_owned()),
            Some("2026-07-31T23:59:59".to_owned()),
            "plan-period",
        );
        let report = build_report(&conn, &scope, None, None, &engine()).expect("report");
        assert!(report.by_project.is_empty());
        assert_eq!(
            stax_memory::pyjson::dumps_http(&report.to_value()),
            r#"{"scope_label":"plan-period","total_cost":0.0,"total_messages":0,"total_sessions":0,"by_project":[]}"#
        );
    }

    #[test]
    fn include_and_exclude_are_none_checks_not_truthiness_checks() {
        let conn = fixture(true);
        conn.execute_batch(
            "INSERT INTO usage_events (project_id, session_id, ts, cost_usd) VALUES
                 (1, 'a', '2026-07-01T00:00:00', 1.0),
                 (2, 'c', '2026-07-01T00:00:00', 2.0);",
        )
        .expect("rows");

        let both = build_report(&conn, &all_time(), None, None, &engine()).expect("report");
        assert_eq!(both.by_project.len(), 2);

        let only_alpha = build_report(
            &conn,
            &all_time(),
            Some(&["alpha".to_owned()]),
            None,
            &engine(),
        )
        .expect("report");
        assert_eq!(only_alpha.by_project.len(), 1);
        assert_eq!(only_alpha.by_project[0].name, "alpha");

        let without_beta = build_report(
            &conn,
            &all_time(),
            None,
            Some(&["beta".to_owned()]),
            &engine(),
        )
        .expect("report");
        assert_eq!(without_beta.by_project.len(), 1);

        // `include=[]` is `is not None`, so it keeps NOTHING — the case a
        // truthiness test would silently turn into "no filter".
        let empty_include =
            build_report(&conn, &all_time(), Some(&[]), None, &engine()).expect("report");
        assert!(empty_include.by_project.is_empty());
        assert_eq!(empty_include.total_cost, 0.0);
    }

    #[test]
    fn the_legacy_path_prices_tokens_and_skips_rows_with_no_model() {
        let conn = fixture(false);
        conn.execute_batch(
            "INSERT INTO messages (session_fk, timestamp, model, input_tokens, output_tokens)
             VALUES
                 (10, '2026-07-01T00:00:00', 'claude-sonnet-4-5', 1000, 500),
                 (10, '2026-07-02T00:00:00', NULL, 9999, 9999),
                 (20, '2026-07-03T00:00:00', '', 9999, 9999);",
        )
        .expect("rows");

        let report = build_report(&conn, &all_time(), None, None, &engine()).expect("report");
        // Every row counts toward `messages`…
        assert_eq!(report.total_messages, 3);
        // …but only the one with a model contributes cost. `COALESCE(model,'')`
        // makes the NULL and the empty string the same falsy value.
        assert!(report.total_cost > 0.0);
        let alpha = report
            .by_project
            .iter()
            .find(|row| row.name == "alpha")
            .expect("alpha");
        let beta = report
            .by_project
            .iter()
            .find(|row| row.name == "beta")
            .expect("beta");
        assert_eq!(beta.cost, 0.0);
        assert_eq!(beta.messages, 1);
        assert!(alpha.cost > 0.0);
        assert_eq!(alpha.sessions, 1);
    }

    #[test]
    fn an_equal_cost_tie_keeps_the_order_the_sql_produced() {
        // `sort(reverse=True)` is stable, so identical costs come out in map
        // order. Sorting ascending and reversing would flip them.
        let conn = fixture(true);
        conn.execute_batch(
            "INSERT INTO usage_events (project_id, session_id, ts, cost_usd) VALUES
                 (1, 'a', '2026-07-01T00:00:00', 3.0),
                 (2, 'c', '2026-07-01T00:00:00', 3.0);",
        )
        .expect("rows");
        let report = build_report(&conn, &all_time(), None, None, &engine()).expect("report");
        assert_eq!(
            report
                .by_project
                .iter()
                .map(|r| r.name.as_str())
                .collect::<Vec<_>>(),
            vec!["alpha", "beta"]
        );
    }

    #[test]
    fn an_empty_string_bound_is_falsy_and_drops_the_clause() {
        // `if since:` — an empty upper bound compared lexicographically would
        // exclude every row; omitting the clause includes them all.
        let conn = fixture(true);
        conn.execute(
            "INSERT INTO usage_events (project_id, session_id, ts, cost_usd) \
             VALUES (1, 'a', '2026-07-01T00:00:00', 4.0)",
            [],
        )
        .expect("insert");
        let scope = Scope::new(Some(String::new()), Some(String::new()), "empty bounds");
        let report = build_report(&conn, &scope, None, None, &engine()).expect("report");
        assert_eq!(report.total_messages, 1);
        assert_eq!(report.total_cost, 4.0);
    }
}
