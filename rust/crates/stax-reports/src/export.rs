//! `reports/export.py` + `reports/render.py` — the cross-project export builder
//! and its two renderers.
//!
//! | Item | Python | Rust |
//! |---|---|---|
//! | `EXPORT_PERIOD_MAP` | `export.py:42` | [`export_period_spec`] |
//! | `DAILY_HEADERS` / `ACTIVITY_HEADERS` | `export.py:50` | [`DAILY_HEADERS`] / [`ACTIVITY_HEADERS`] |
//! | `build_period_export` | `export.py:72` | [`build_period_export`] |
//! | `build_multi_period_export` | `export.py:148` | [`build_multi_period_export`] |
//! | `build_export_payload` | `export.py:748` | [`build_export_payload`] |
//! | `render_export_payload` | `export.py:789` | [`render_export_payload`] |
//! | `run_export` | `export.py:803` | [`run_export`] |
//! | `render_export_csv` | `render.py:94` | [`render_export_csv`] |
//! | `render_export_json` | `render.py:146` | [`render_export_json`] |
//!
//! `stax export` (the CLI verb) and `GET /api/export` both call
//! `run_export`, which is why the whole thing lives here rather than inside
//! `routes/export.rs`: wave 8 ports the CLI verb and must find this, not fork it.
//!
//! # This is the only endpoint in the wave whose body is not JSON
//!
//! `run_export` returns `(text, content_type, filename)` and the route wraps it
//! in a bare starlette `Response`, so **neither** branch goes through
//! `JSONResponse`. That matters for the `format=json` branch specifically: its
//! body is `json.dumps(payload, indent=2, sort_keys=False, default=str)`, i.e.
//! `ensure_ascii=`**`True`** and the two-space pretty layout — the *CLI* writer,
//! not the HTTP one. So [`render_export_json`] calls
//! [`stax_memory::pyjson::dumps_pretty`] and not `dumps_http`, and a project
//! named `café` ships `café` here while the very same string ships as raw
//! UTF-8 from `/api/projects`. Measured against the reference, not inferred.
//! (`default=str` never fires — every leaf in the payload is `str`, `int`,
//! `float` or `None` — so it needs no counterpart here.)
//!
//! # What is load-bearing, and what merely looks it
//!
//! * **The CSV is CPython's `csv` module, not "commas and newlines".** Quoting
//!   is `QUOTE_MINIMAL` with `lineterminator="\n"`, and the trigger set was
//!   measured rather than read off the dialect: `,`, `"`, `\n` **and `\r`** —
//!   the carriage return quotes even though it is not in the line terminator,
//!   because `_csv.c` tests `c == '\r' || c == '\n'` on top of the terminator
//!   scan. A row consisting of one empty field renders as `""`, not as nothing.
//!   See [`CsvWriter`].
//! * **`deep` is `fmt == "json"`.** The CSV branch therefore *never* runs
//!   [`deep_breakdowns`], and its `# activity` section is always a header with
//!   zero rows. That is not a bug to route around — it is the reason a CSV
//!   export is cheap and a JSON export re-runs the whole aggregator pipeline
//!   once per in-scope project.
//! * **The two sorts round *after* they sort.** `daily` and `projects` are
//!   ordered on the unrounded `cost_usd` and only then have it replaced with
//!   `round(x, 6)`. Rounding first would reorder ties.
//! * **`sorted(..., reverse=True)` is stable.** The per-project order is by cost
//!   descending with ties in *first-seen* order, which is the order the
//!   `ORDER BY day, slug` sweep inserted them in. A `HashMap` here would make
//!   the response order vary per run on any store with two equal-cost projects,
//!   so the roll-ups are insertion-ordered ([`Ordered`]).
//! * **`Counter.most_common()` is the same stable sort**, so the activity /
//!   tool / MCP lists tie-break on first-seen too.
//!
//! # The clock, and which case rows can ever be green
//!
//! `run_export` reads the clock **twice** and the reads are ordered:
//!
//! 1. the payload build — `build_multi_period_export`'s single
//!    `datetime.now(UTC)` (shared by all three windows *and* by `generated`),
//!    or `parse_period`'s own read for a single window;
//! 2. `datetime.now(UTC).strftime("%Y-%m-%d")` for the filename, **after** the
//!    payload.
//!
//! Both are injected as a `clock` closure so the order is visible and a test
//! can pin them. What that costs the differ:
//!
//! * `period=today` / `month` / `all` — stable within a calendar day, byte-diffable.
//! * `period=week` — `7days` is `now - timedelta(days=7)` and *carries the
//!   microsecond*, which lands in the payload as `since` / `until`. Two servers,
//!   two microsecond values: the JSON body can **never** match. The CSV body
//!   does not render the bounds, so it matches unless a message falls in the
//!   millisecond gap.
//! * no `period` at all — the rollup embeds `generated` (a full ISO instant)
//!   *and* `last_7d` / `last_30d` bounds. JSON can never match; CSV can.
//!
//! # Bug-for-bug
//!
//! `totals.sessions` is `sum(r["sessions"] for r in daily)` — a sum of per-day
//! distinct counts, so a session spanning three days counts three times, while
//! `projects[].sessions` on the same payload is a true distinct count. The two
//! disagree by construction and Python says so in a comment
//! (`"we approximate with…"`). Ported as written.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use rusqlite::Connection;
use serde_json::{Map, Value};
use stax_etl::pricing::RawTokens;
use stax_etl::pricing::costs::PricingEngine;
// `round_py` is Python's `round(x, n)`; this file used to carry a private copy
// under the name below, which is kept so the call sites read unchanged.
use stax_etl::stats::aggregator::{Neumaier, round_py as round_half_even};
use stax_etl::stats::pytext::{is_py_space, py_char_prefix, py_lstrip, py_strip, py_truthy};

use crate::scope::{Instant, Scope, parse_period};

/// `DAILY_HEADERS` — the CSV daily section's column order, and the key order of
/// every `daily` row in the JSON payload.
pub const DAILY_HEADERS: [&str; 10] = [
    "date",
    "provider",
    "project",
    "cost_usd",
    "calls",
    "sessions",
    "input_tokens",
    "output_tokens",
    "cache_read_tokens",
    "cache_write_tokens",
];

/// `ACTIVITY_HEADERS` — note that the first column is `activity` while the JSON
/// key for the same field is `name`. Both spellings are the public contract.
pub const ACTIVITY_HEADERS: [&str; 3] = ["activity", "calls", "share_pct"];

/// `EXPORT_PERIOD_MAP` — the user-facing period name → `scope.parse_period` spec.
///
/// A list rather than a map because two orders matter and neither is a hash
/// order: this one is the literal's, and `sorted(EXPORT_PERIOD_MAP)` is what the
/// `ValueError` message interpolates.
const EXPORT_PERIOD_MAP: [(&str, &str); 4] = [
    ("today", "today"),
    ("week", "7days"),
    ("month", "30days"),
    ("all", "all"),
];

/// `EXPORT_PERIOD_MAP[period]`, or `None` for an unknown name.
#[must_use]
pub fn export_period_spec(period: &str) -> Option<&'static str> {
    EXPORT_PERIOD_MAP
        .iter()
        .find(|(name, _)| *name == period)
        .map(|(_, spec)| *spec)
}

/// What `run_export` hands the caller: `(text, content_type, filename)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Export {
    /// The rendered body. CSV or pretty JSON — never a `JSONResponse` render.
    pub text: String,
    /// `"text/csv"` or `"application/json"`, before starlette's charset rule.
    pub content_type: &'static str,
    /// `stackunderflow-export-<period|rollup>-<YYYY-MM-DD>.<fmt>`.
    pub filename: String,
}

/// The two failure modes Python distinguishes: a `ValueError` the route turns
/// into a `400`, and anything else (SQLite, the manifest) which becomes a `500`.
#[derive(Debug, Clone)]
pub enum ExportError {
    /// `raise ValueError(...)` — `routes/export.py` catches this one by name.
    Value(String),
    /// Everything that would have propagated out of the handler.
    Internal(String),
}

impl std::fmt::Display for ExportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Value(msg) | Self::Internal(msg) => f.write_str(msg),
        }
    }
}

impl From<rusqlite::Error> for ExportError {
    fn from(err: rusqlite::Error) -> Self {
        Self::Internal(err.to_string())
    }
}

// ── the facade both the CLI verb and the route call ──────────────────────────

/// `run_export(conn, fmt=…, period=…, provider=…, include=…, exclude=…)`.
///
/// `clock` stands in for the two `datetime.now(UTC)` calls, **in Python's
/// order**: the payload build reads it first, the filename second. See the
/// module docs for why that ordering is worth preserving.
///
/// # Errors
/// [`ExportError::Value`] for an unknown `period` or `fmt` (both unreachable
/// from the HTTP route, which validates first); [`ExportError::Internal`] for a
/// SQLite failure.
#[allow(
    clippy::too_many_arguments,
    reason = "mirrors run_export's own keyword-argument list"
)]
pub fn run_export(
    conn: &Connection,
    engine: &PricingEngine,
    fmt: &str,
    period: Option<&str>,
    provider: Option<&str>,
    include: Option<&[String]>,
    exclude: Option<&[String]>,
    clock: &dyn Fn() -> Instant,
) -> Result<Export, ExportError> {
    // `deep=(fmt == "json")` — the single most expensive flag in this module.
    let payload = build_export_payload(
        conn,
        engine,
        period,
        provider,
        include,
        exclude,
        fmt == "json",
        clock,
    )?;
    let text = render_export_payload(&payload, fmt)?;

    // `datetime.now(UTC).strftime("%Y-%m-%d")`. The first ten characters of an
    // aware `isoformat()` are exactly that, and both pad the year to four.
    let stamp = clock().isoformat();
    let today = &stamp[..10];
    // `label = period or "rollup"` — Python truthiness, so `?period=` (the empty
    // string) would also be "rollup"… except the route rejects it at the
    // allow-list first, so only `None` reaches here.
    let label = period.filter(|p| !p.is_empty()).unwrap_or("rollup");
    let filename = format!("stackunderflow-export-{label}-{today}.{fmt}");
    let content_type = if fmt == "csv" {
        "text/csv"
    } else {
        "application/json"
    };
    Ok(Export {
        text,
        content_type,
        filename,
    })
}

/// `build_export_payload` — `period is None` is the multi-period rollup.
///
/// # Errors
/// [`ExportError::Value`] for a `period` outside [`EXPORT_PERIOD_MAP`];
/// [`ExportError::Internal`] for a SQLite failure.
#[allow(
    clippy::too_many_arguments,
    reason = "mirrors build_export_payload's own keyword-argument list"
)]
pub fn build_export_payload(
    conn: &Connection,
    engine: &PricingEngine,
    period: Option<&str>,
    provider: Option<&str>,
    include: Option<&[String]>,
    exclude: Option<&[String]>,
    deep: bool,
    clock: &dyn Fn() -> Instant,
) -> Result<Value, ExportError> {
    let Some(period) = period else {
        return build_multi_period_export(conn, engine, provider, include, exclude, deep, clock());
    };
    let Some(spec) = export_period_spec(period) else {
        // `", ".join(sorted(EXPORT_PERIOD_MAP))` — sorted over the KEYS.
        let mut names: Vec<&str> = EXPORT_PERIOD_MAP.iter().map(|(name, _)| *name).collect();
        names.sort_unstable();
        return Err(ExportError::Value(format!(
            "Unknown period '{period}'. Valid: {}",
            names.join(", ")
        )));
    };
    // `parse_period(spec)` with no `now=` — its own clock read, which is why the
    // closure is called here rather than hoisted.
    let scope = parse_period(spec, clock()).map_err(ExportError::Value)?;
    build_period_export(conn, engine, &scope, provider, include, exclude, deep)
}

/// `render_export_payload(payload, fmt)`.
///
/// # Errors
/// [`ExportError::Value`] for a format that is neither `csv` nor `json`.
pub fn render_export_payload(payload: &Value, fmt: &str) -> Result<String, ExportError> {
    match fmt {
        "csv" => Ok(render_export_csv(payload)),
        "json" => Ok(render_export_json(payload)),
        _ => Err(ExportError::Value(format!(
            "Unknown format '{fmt}'. Valid: csv, json"
        ))),
    }
}

// ── builders ─────────────────────────────────────────────────────────────────

/// `build_multi_period_export` — today + last_7d + last_30d under one roof.
///
/// **One clock read for all three windows and for `generated`.** Python takes
/// `current = now or datetime.now(UTC)` once and threads it into each
/// `parse_period(..., now=current)`, so the three scopes are exactly 0 / 7 / 30
/// days apart rather than three independent samples. Splitting it would be a
/// silent behaviour change on a slow store.
///
/// # Errors
/// A SQLite failure in any of the three sub-builds.
pub fn build_multi_period_export(
    conn: &Connection,
    engine: &PricingEngine,
    provider: Option<&str>,
    include: Option<&[String]>,
    exclude: Option<&[String]>,
    deep: bool,
    now: Instant,
) -> Result<Value, ExportError> {
    let today = parse_period("today", now).map_err(ExportError::Value)?;
    let week = parse_period("7days", now).map_err(ExportError::Value)?;
    let month = parse_period("30days", now).map_err(ExportError::Value)?;

    let mut filters = Map::new();
    filters.insert("provider".to_owned(), opt_str(provider));
    filters.insert("include".to_owned(), opt_list(include));
    filters.insert("exclude".to_owned(), opt_list(exclude));

    let mut out = Map::new();
    out.insert("schema".to_owned(), Value::from("stackunderflow.export.v1"));
    // The wall clock, verbatim in the body — see the module docs on which case
    // rows this makes undiffable.
    out.insert("generated".to_owned(), Value::from(now.isoformat()));
    out.insert("filters".to_owned(), Value::Object(filters));
    out.insert(
        "today".to_owned(),
        build_period_export(conn, engine, &today, provider, include, exclude, deep)?,
    );
    out.insert(
        "last_7d".to_owned(),
        build_period_export(conn, engine, &week, provider, include, exclude, deep)?,
    );
    out.insert(
        "last_30d".to_owned(),
        build_period_export(conn, engine, &month, provider, include, exclude, deep)?,
    );
    Ok(Value::Object(out))
}

/// `build_period_export(conn, scope=…, …)` — one window.
///
/// The eleven keys go out in the literal's order, which is the wire contract.
///
/// # Errors
/// A SQLite failure in any of the sweeps.
pub fn build_period_export(
    conn: &Connection,
    engine: &PricingEngine,
    scope: &Scope,
    provider: Option<&str>,
    include: Option<&[String]>,
    exclude: Option<&[String]>,
    deep: bool,
) -> Result<Value, ExportError> {
    let since = scope.since.as_deref();
    let until = scope.until.as_deref();

    let (daily, projects) =
        build_daily_and_projects(conn, engine, since, until, provider, include, exclude)?;
    let totals = totals_from_daily(&daily);
    // Computed even for CSV, which never renders it. Python does the same sweep
    // unconditionally; skipping it here would be an optimisation, i.e. a
    // divergence in everything but the bytes.
    let models = models_from_messages(conn, engine, since, until, provider, include, exclude)?;

    let (activities, tools, mcp_calls, shell) = if deep {
        deep_breakdowns(conn, engine, scope, provider, include, exclude)?
    } else {
        (Vec::new(), Vec::new(), Vec::new(), Vec::new())
    };

    let mut out = Map::new();
    out.insert("label".to_owned(), Value::from(scope.label.clone()));
    out.insert("since".to_owned(), opt_str(since));
    out.insert("until".to_owned(), opt_str(until));
    out.insert("totals".to_owned(), totals);
    out.insert(
        "daily".to_owned(),
        Value::Array(daily.iter().map(DailyRow::to_json).collect()),
    );
    out.insert(
        "projects".to_owned(),
        Value::Array(projects.iter().map(ProjectRow::to_json).collect()),
    );
    out.insert("models".to_owned(), models);
    out.insert("activities".to_owned(), Value::Array(activities));
    out.insert("tools".to_owned(), Value::Array(tools));
    out.insert("mcp".to_owned(), Value::Array(mcp_calls));
    out.insert("shell".to_owned(), Value::Array(shell));
    Ok(Value::Object(out))
}

// ── internals: the grouped sweep ─────────────────────────────────────────────

/// One `daily_map` value — the field order below IS the JSON key order.
#[derive(Debug, Clone, Default)]
struct DailyRow {
    date: String,
    provider: String,
    project: String,
    cost_usd: f64,
    calls: i64,
    sessions: i64,
    input_tokens: i64,
    output_tokens: i64,
    cache_read_tokens: i64,
    cache_write_tokens: i64,
}

impl DailyRow {
    fn to_json(&self) -> Value {
        let mut obj = Map::new();
        obj.insert("date".to_owned(), Value::from(self.date.clone()));
        obj.insert("provider".to_owned(), Value::from(self.provider.clone()));
        obj.insert("project".to_owned(), Value::from(self.project.clone()));
        obj.insert("cost_usd".to_owned(), jf(self.cost_usd));
        obj.insert("calls".to_owned(), Value::from(self.calls));
        obj.insert("sessions".to_owned(), Value::from(self.sessions));
        obj.insert("input_tokens".to_owned(), Value::from(self.input_tokens));
        obj.insert("output_tokens".to_owned(), Value::from(self.output_tokens));
        obj.insert(
            "cache_read_tokens".to_owned(),
            Value::from(self.cache_read_tokens),
        );
        obj.insert(
            "cache_write_tokens".to_owned(),
            Value::from(self.cache_write_tokens),
        );
        Value::Object(obj)
    }
}

/// One `project_map` value. Same fields as [`DailyRow`] minus `date`, and the
/// slug is called `name` here and `project` there — both spellings ship.
#[derive(Debug, Clone, Default)]
struct ProjectRow {
    name: String,
    provider: String,
    cost_usd: f64,
    calls: i64,
    sessions: i64,
    input_tokens: i64,
    output_tokens: i64,
    cache_read_tokens: i64,
    cache_write_tokens: i64,
}

impl ProjectRow {
    fn to_json(&self) -> Value {
        let mut obj = Map::new();
        obj.insert("name".to_owned(), Value::from(self.name.clone()));
        obj.insert("provider".to_owned(), Value::from(self.provider.clone()));
        obj.insert("cost_usd".to_owned(), jf(self.cost_usd));
        obj.insert("calls".to_owned(), Value::from(self.calls));
        obj.insert("sessions".to_owned(), Value::from(self.sessions));
        obj.insert("input_tokens".to_owned(), Value::from(self.input_tokens));
        obj.insert("output_tokens".to_owned(), Value::from(self.output_tokens));
        obj.insert(
            "cache_read_tokens".to_owned(),
            Value::from(self.cache_read_tokens),
        );
        obj.insert(
            "cache_write_tokens".to_owned(),
            Value::from(self.cache_write_tokens),
        );
        Value::Object(obj)
    }
}

/// The `(provider, slug, day, model, speed)` sweep, collapsed twice.
///
/// The SQL is transcribed rather than improved. Two JOINs, not a subquery,
/// because that is what Python writes here — §6b's list-subquery rule applies
/// where the *reference* uses one, and this query does not.
#[allow(
    clippy::too_many_arguments,
    reason = "mirrors _build_daily_and_projects' own keyword-argument list"
)]
fn build_daily_and_projects(
    conn: &Connection,
    engine: &PricingEngine,
    since: Option<&str>,
    until: Option<&str>,
    provider: Option<&str>,
    include: Option<&[String]>,
    exclude: Option<&[String]>,
) -> Result<(Vec<DailyRow>, Vec<ProjectRow>), ExportError> {
    let mut sql = String::from(
        "SELECT projects.provider AS provider, \
                projects.slug AS slug, \
                substr(messages.timestamp, 1, 10) AS day, \
                COALESCE(messages.model, '') AS model, \
                COALESCE(messages.speed, 'standard') AS speed, \
                SUM(messages.input_tokens)        AS in_tok, \
                SUM(messages.output_tokens)       AS out_tok, \
                SUM(messages.cache_read_tokens)   AS cache_r, \
                SUM(messages.cache_create_tokens) AS cache_w, \
                COUNT(*) AS calls, \
                COUNT(DISTINCT messages.session_fk) AS sessions \
         FROM messages \
         JOIN sessions ON sessions.id = messages.session_fk \
         JOIN projects ON projects.id = sessions.project_id \
         WHERE 1=1 ",
    );
    let params = push_scope_filters(&mut sql, since, until, provider);
    sql.push_str("GROUP BY provider, slug, day, model, speed ORDER BY day, slug");

    let inc = as_set(include);
    let exc = as_set(exclude);

    let mut daily: Ordered<(String, String, String), DailyRow> = Ordered::default();
    let mut projects: Ordered<String, ProjectRow> = Ordered::default();

    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query(rusqlite::params_from_iter(params.iter()))?;
    while let Some(row) = rows.next()? {
        let slug: String = row.get("slug")?;
        if inc.as_ref().is_some_and(|set| !set.contains(&slug)) {
            continue;
        }
        if exc.as_ref().is_some_and(|set| set.contains(&slug)) {
            continue;
        }
        // `r["provider"] or ""` / `r["day"] or ""` — NULL *and* "" both fall to "".
        let prov: String = row
            .get::<_, Option<String>>("provider")?
            .unwrap_or_default();
        let day: String = row.get::<_, Option<String>>("day")?.unwrap_or_default();
        let model: String = row.get("model")?;
        // `r["speed"] or "standard"` — the COALESCE already handles NULL; this
        // second guard catches the empty string the column can legally hold.
        let speed: String = row.get("speed")?;
        let speed = if speed.is_empty() {
            "standard".to_owned()
        } else {
            speed
        };
        let in_tok = row.get::<_, Option<i64>>("in_tok")?.unwrap_or(0);
        let out_tok = row.get::<_, Option<i64>>("out_tok")?.unwrap_or(0);
        let cache_r = row.get::<_, Option<i64>>("cache_r")?.unwrap_or(0);
        let cache_w = row.get::<_, Option<i64>>("cache_w")?.unwrap_or(0);
        let calls = row.get::<_, Option<i64>>("calls")?.unwrap_or(0);

        // `if model:` — an empty model name costs nothing and is not priced.
        let cost = if model.is_empty() {
            0.0
        } else {
            price(
                engine, in_tok, out_tok, cache_r, cache_w, &model, &prov, &speed,
            )
        };

        let entry = daily.entry((day.clone(), prov.clone(), slug.clone()), || DailyRow {
            date: day.clone(),
            provider: prov.clone(),
            project: slug.clone(),
            ..DailyRow::default()
        });
        // `d["cost_usd"] += cost` — a plain `+=`, NOT `sum()`. §6b law 3: match
        // the operation, not the accuracy.
        entry.cost_usd += cost;
        entry.calls += calls;
        entry.input_tokens += in_tok;
        entry.output_tokens += out_tok;
        entry.cache_read_tokens += cache_r;
        entry.cache_write_tokens += cache_w;

        let entry = projects.entry(slug.clone(), || ProjectRow {
            name: slug.clone(),
            // NOTE: first-seen provider wins. A slug that exists under two
            // providers reports whichever the `ORDER BY day, slug` sweep hit
            // first, and its counts are the SUM over both. Python's
            // `setdefault` does exactly this.
            provider: prov.clone(),
            ..ProjectRow::default()
        });
        entry.cost_usd += cost;
        entry.calls += calls;
        entry.input_tokens += in_tok;
        entry.output_tokens += out_tok;
        entry.cache_read_tokens += cache_r;
        entry.cache_write_tokens += cache_w;
    }
    drop(rows);
    drop(stmt);

    populate_session_counts(
        conn,
        &mut daily,
        &mut projects,
        since,
        until,
        provider,
        inc.as_ref(),
        exc.as_ref(),
    )?;

    let mut daily = daily.into_values();
    // `key=lambda r: (r["date"], r["provider"], r["project"])` — which is the
    // map key, so the order is total and the sort's stability is moot.
    daily.sort_by(|a, b| {
        (&a.date, &a.provider, &a.project).cmp(&(&b.date, &b.provider, &b.project))
    });
    for row in &mut daily {
        row.cost_usd = round_half_even(row.cost_usd, 6);
    }

    let mut projects = projects.into_values();
    // `sorted(..., key=cost, reverse=True)`: CPython reverses, stable-sorts,
    // reverses again, so equal costs keep their INSERTION order. Rust's
    // `sort_by` is stable, so comparing `b` against `a` reproduces it exactly —
    // sorting ascending and then reversing would not.
    projects.sort_by(|a, b| {
        b.cost_usd
            .partial_cmp(&a.cost_usd)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    for row in &mut projects {
        row.cost_usd = round_half_even(row.cost_usd, 6);
    }

    Ok((daily, projects))
}

/// `_populate_session_counts` — the distinct-session pass the grouped query
/// above cannot do.
///
/// `COUNT(DISTINCT session_fk)` in the `(day, model, speed)` sweep counts within
/// a bucket, so a session that used two models on one day would be counted
/// twice. Hence two more queries: one grouped to `(provider, slug, day)` for the
/// daily rows, one enumerating `session_id`s for the per-project set.
#[allow(
    clippy::too_many_arguments,
    reason = "mirrors _populate_session_counts' own keyword-argument list"
)]
fn populate_session_counts(
    conn: &Connection,
    daily: &mut Ordered<(String, String, String), DailyRow>,
    projects: &mut Ordered<String, ProjectRow>,
    since: Option<&str>,
    until: Option<&str>,
    provider: Option<&str>,
    include: Option<&HashSet<String>>,
    exclude: Option<&HashSet<String>>,
) -> Result<(), ExportError> {
    let mut sql = String::from(
        "SELECT projects.provider AS provider, \
                projects.slug AS slug, \
                substr(messages.timestamp, 1, 10) AS day, \
                COUNT(DISTINCT messages.session_fk) AS sessions \
         FROM messages \
         JOIN sessions ON sessions.id = messages.session_fk \
         JOIN projects ON projects.id = sessions.project_id \
         WHERE 1=1 ",
    );
    let params = push_scope_filters(&mut sql, since, until, provider);
    sql.push_str("GROUP BY provider, slug, day");

    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query(rusqlite::params_from_iter(params.iter()))?;
    while let Some(row) = rows.next()? {
        let slug: String = row.get("slug")?;
        if include.is_some_and(|set| !set.contains(&slug)) {
            continue;
        }
        if exclude.is_some_and(|set| set.contains(&slug)) {
            continue;
        }
        let day: String = row.get::<_, Option<String>>("day")?.unwrap_or_default();
        let prov: String = row
            .get::<_, Option<String>>("provider")?
            .unwrap_or_default();
        let sessions = row.get::<_, Option<i64>>("sessions")?.unwrap_or(0);
        // `if key in daily_map:` — a key this sweep produces but the grouped one
        // did not is silently dropped, never inserted.
        if let Some(entry) = daily.get_mut(&(day, prov, slug)) {
            entry.sessions = sessions;
        }
    }
    drop(rows);
    drop(stmt);

    let mut sql = String::from(
        "SELECT projects.provider AS provider, \
                projects.slug AS slug, \
                sessions.session_id AS sid \
         FROM messages \
         JOIN sessions ON sessions.id = messages.session_fk \
         JOIN projects ON projects.id = sessions.project_id \
         WHERE 1=1 ",
    );
    let params = push_scope_filters(&mut sql, since, until, provider);
    sql.push_str("GROUP BY provider, slug, sessions.session_id");

    // `defaultdict(set)` keyed on the SLUG only — so two providers sharing a
    // slug pool their sessions here, while the grouped sweeps keep them apart.
    let mut per_project: HashMap<String, HashSet<String>> = HashMap::new();
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query(rusqlite::params_from_iter(params.iter()))?;
    while let Some(row) = rows.next()? {
        let slug: String = row.get("slug")?;
        if include.is_some_and(|set| !set.contains(&slug)) {
            continue;
        }
        if exclude.is_some_and(|set| set.contains(&slug)) {
            continue;
        }
        let sid: Option<String> = row.get("sid")?;
        per_project
            .entry(slug)
            .or_default()
            .insert(sid.unwrap_or_default());
    }
    drop(rows);
    drop(stmt);

    // `for slug, p in project_map.items(): p["sessions"] = len(...)` — every
    // project is stamped, including the ones with no row here (which get 0).
    for (slug, entry) in projects.iter_mut() {
        entry.sessions = per_project
            .get(slug)
            .map_or(0, |set| i64::try_from(set.len()).unwrap_or(i64::MAX));
    }
    Ok(())
}

/// `_totals_from_daily` — seven `sum()`s and one set comprehension.
fn totals_from_daily(daily: &[DailyRow]) -> Value {
    let mut obj = Map::new();
    if daily.is_empty() {
        // The literal's zeros: `cost_usd` is a FLOAT `0.0` and everything else
        // is an INT `0`. `json.dumps` writes `0.0` and `0` respectively, and the
        // differ reads both (DIV-057).
        obj.insert("cost_usd".to_owned(), jf(0.0));
        for key in [
            "calls",
            "sessions",
            "input_tokens",
            "output_tokens",
            "cache_read_tokens",
            "cache_write_tokens",
            "projects",
        ] {
            obj.insert(key.to_owned(), Value::from(0));
        }
        return Value::Object(obj);
    }
    // `sum(r["cost_usd"] for r in daily)` — CPython 3.12's compensated float
    // path (gh-100425), not a `+=` chain. The list is non-empty here, so `sum()`
    // cannot return the `int` 0 that `finish_pynum` guards against.
    let mut cost = Neumaier::default();
    for row in daily {
        cost.add(row.cost_usd);
    }
    let calls: i64 = daily.iter().map(|r| r.calls).sum();
    let in_tok: i64 = daily.iter().map(|r| r.input_tokens).sum();
    let out_tok: i64 = daily.iter().map(|r| r.output_tokens).sum();
    let cache_r: i64 = daily.iter().map(|r| r.cache_read_tokens).sum();
    let cache_w: i64 = daily.iter().map(|r| r.cache_write_tokens).sum();
    // See the module docs: this double-counts a session that spans two days, and
    // the sibling `projects[].sessions` does not. Both ship.
    let sessions: i64 = daily.iter().map(|r| r.sessions).sum();
    let distinct: HashSet<(&str, &str)> = daily
        .iter()
        .map(|r| (r.provider.as_str(), r.project.as_str()))
        .collect();

    obj.insert("cost_usd".to_owned(), jf(round_half_even(cost.finish(), 6)));
    obj.insert("calls".to_owned(), Value::from(calls));
    obj.insert("sessions".to_owned(), Value::from(sessions));
    obj.insert("input_tokens".to_owned(), Value::from(in_tok));
    obj.insert("output_tokens".to_owned(), Value::from(out_tok));
    obj.insert("cache_read_tokens".to_owned(), Value::from(cache_r));
    obj.insert("cache_write_tokens".to_owned(), Value::from(cache_w));
    obj.insert(
        "projects".to_owned(),
        Value::from(i64::try_from(distinct.len()).unwrap_or(i64::MAX)),
    );
    Value::Object(obj)
}

/// One `models` bucket, before it becomes JSON.
struct ModelRoll {
    calls: i64,
    cost_usd: f64,
    input_tokens: i64,
    output_tokens: i64,
    cache_read_tokens: i64,
    cache_write_tokens: i64,
}

/// `_models_from_messages` — grouped by `(model, speed)`, keyed by model alone.
///
/// The speed dimension exists only so Anthropic's priority tier prices at its
/// multiplier; the output collapses it, so one model's `fast` and `standard`
/// rows land in the same bucket with different rates already applied.
#[allow(
    clippy::too_many_arguments,
    reason = "mirrors _models_from_messages' own keyword-argument list"
)]
fn models_from_messages(
    conn: &Connection,
    engine: &PricingEngine,
    since: Option<&str>,
    until: Option<&str>,
    provider: Option<&str>,
    include: Option<&[String]>,
    exclude: Option<&[String]>,
) -> Result<Value, ExportError> {
    let mut sql = String::from(
        "SELECT projects.provider AS provider, \
                projects.slug AS slug, \
                COALESCE(messages.model, '') AS model, \
                COALESCE(messages.speed, 'standard') AS speed, \
                SUM(messages.input_tokens)        AS in_tok, \
                SUM(messages.output_tokens)       AS out_tok, \
                SUM(messages.cache_read_tokens)   AS cache_r, \
                SUM(messages.cache_create_tokens) AS cache_w, \
                COUNT(*) AS calls \
         FROM messages \
         JOIN sessions ON sessions.id = messages.session_fk \
         JOIN projects ON projects.id = sessions.project_id \
         WHERE 1=1 ",
    );
    let params = push_scope_filters(&mut sql, since, until, provider);
    // NOTE: no ORDER BY. The output dict's key order is SQLite's row order for
    // this GROUP BY, which both implementations get from the same planner over
    // the same file — the reason a `HashMap` here would be wrong even though the
    // order is not *specified*.
    sql.push_str("GROUP BY provider, slug, model, speed");

    let inc = as_set(include);
    let exc = as_set(exclude);

    let mut out: Ordered<String, ModelRoll> = Ordered::default();
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query(rusqlite::params_from_iter(params.iter()))?;
    while let Some(row) = rows.next()? {
        let slug: String = row.get("slug")?;
        if inc.as_ref().is_some_and(|set| !set.contains(&slug)) {
            continue;
        }
        if exc.as_ref().is_some_and(|set| set.contains(&slug)) {
            continue;
        }
        let model: String = row.get("model")?;
        // `if not model: continue` — the unnamed-model bucket is dropped here
        // and KEPT in the daily rows. The two sections disagree on purpose.
        if model.is_empty() {
            continue;
        }
        let speed: String = row.get("speed")?;
        let speed = if speed.is_empty() {
            "standard".to_owned()
        } else {
            speed
        };
        let in_tok = row.get::<_, Option<i64>>("in_tok")?.unwrap_or(0);
        let out_tok = row.get::<_, Option<i64>>("out_tok")?.unwrap_or(0);
        let cache_r = row.get::<_, Option<i64>>("cache_r")?.unwrap_or(0);
        let cache_w = row.get::<_, Option<i64>>("cache_w")?.unwrap_or(0);
        let calls = row.get::<_, Option<i64>>("calls")?.unwrap_or(0);
        let prov: String = row
            .get::<_, Option<String>>("provider")?
            .unwrap_or_default();

        let cost = price(
            engine, in_tok, out_tok, cache_r, cache_w, &model, &prov, &speed,
        );

        let entry = out.entry(model, || ModelRoll {
            calls: 0,
            cost_usd: 0.0,
            input_tokens: 0,
            output_tokens: 0,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
        });
        entry.calls += calls;
        entry.cost_usd += cost;
        entry.input_tokens += in_tok;
        entry.output_tokens += out_tok;
        entry.cache_read_tokens += cache_r;
        entry.cache_write_tokens += cache_w;
    }
    drop(rows);
    drop(stmt);

    let mut obj = Map::new();
    for (model, roll) in out.into_pairs() {
        let mut entry = Map::new();
        entry.insert("calls".to_owned(), Value::from(roll.calls));
        entry.insert("cost_usd".to_owned(), jf(round_half_even(roll.cost_usd, 6)));
        entry.insert("input_tokens".to_owned(), Value::from(roll.input_tokens));
        entry.insert("output_tokens".to_owned(), Value::from(roll.output_tokens));
        entry.insert(
            "cache_read_tokens".to_owned(),
            Value::from(roll.cache_read_tokens),
        );
        entry.insert(
            "cache_write_tokens".to_owned(),
            Value::from(roll.cache_write_tokens),
        );
        obj.insert(model, Value::Object(entry));
    }
    Ok(Value::Object(obj))
}

// ── internals: the deep (JSON-only) breakdowns ───────────────────────────────

/// `_deep_breakdowns` — the whole aggregator pipeline, once per in-scope project.
///
/// This is the expensive half of a JSON export and there is no cheaper shape
/// available: the tool and command counts it wants live in
/// `aggregator.summarise`'s output, which means reconstructing every `RawEntry`
/// from `messages.raw_json` and running dedup → classify → enrich → aggregate
/// for each candidate. On the maintainer's store that is minutes for
/// `period=all` with no project filter. Python has the same cost; the fix (a
/// mart-backed tool rollup) is a product change, not a port decision.
///
/// A failure in one project is swallowed (`except Exception: continue`), so a
/// single corrupt `raw_json` degrades that project's contribution to nothing
/// rather than 500-ing the export.
#[allow(
    clippy::type_complexity,
    reason = "the 4-tuple is Python's return shape"
)]
fn deep_breakdowns(
    conn: &Connection,
    engine: &PricingEngine,
    scope: &Scope,
    provider: Option<&str>,
    include: Option<&[String]>,
    exclude: Option<&[String]>,
) -> Result<(Vec<Value>, Vec<Value>, Vec<Value>, Vec<Value>), ExportError> {
    let inc = as_set(include);
    let exc = as_set(exclude);

    let mut sql = String::from(
        "SELECT DISTINCT projects.id AS id, projects.slug AS slug, \
                projects.provider AS provider \
         FROM projects \
         JOIN sessions ON sessions.project_id = projects.id \
         JOIN messages ON messages.session_fk = sessions.id \
         WHERE 1=1 ",
    );
    let params = push_scope_filters(
        &mut sql,
        scope.since.as_deref(),
        scope.until.as_deref(),
        provider,
    );

    let mut candidates: Vec<i64> = Vec::new();
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query(rusqlite::params_from_iter(params.iter()))?;
    while let Some(row) = rows.next()? {
        let slug: String = row.get("slug")?;
        if inc.as_ref().is_some_and(|set| !set.contains(&slug)) {
            continue;
        }
        if exc.as_ref().is_some_and(|set| set.contains(&slug)) {
            continue;
        }
        candidates.push(row.get("id")?);
    }
    drop(rows);
    drop(stmt);

    let mut tool_counts = Counter::default();
    let mut mcp_counts = Counter::default();
    let mut bash_total: i64 = 0;
    let mut cmd_counts = Counter::default();

    for project_id in candidates {
        // `queries.get_project_stats(conn, project_id=…)` — tz_offset defaults
        // to 0 and the engine is INJECTED (§6b law 2: never `default_engine()`
        // in a server path, DIV-056).
        let Ok((_messages, stats)) =
            stax_etl::stats::dataset::get_project_stats_with(conn, &[project_id], 0, engine)
        else {
            continue;
        };
        // `if not stats: continue` — an absent project returns `{}`, which is
        // falsy.
        if !py_truthy(&stats) {
            continue;
        }

        // `stats.get("tools", {}) or {}` then `.get("usage_counts") or {}`.
        if let Some(usage) = stats
            .get("tools")
            .and_then(Value::as_object)
            .and_then(|tools| tools.get("usage_counts"))
            .and_then(Value::as_object)
        {
            for (name, count) in usage {
                // `int(n or 0)` — `null`, `0` and a missing value all give 0.
                let count = count.as_i64().unwrap_or(0);
                if name.starts_with("mcp__") {
                    mcp_counts.incr(name, count);
                } else if name == "Bash" {
                    // Python parks this under a `__bash_total__` sentinel key in
                    // a Counter it never reads any other key of — so it is a
                    // scalar here, which is what it always was.
                    bash_total += count;
                } else {
                    tool_counts.incr(name, count);
                }
            }
        }

        let details = stats
            .get("user_interactions")
            .and_then(Value::as_object)
            .and_then(|ui| ui.get("command_details"))
            .and_then(Value::as_array);
        for detail in details.into_iter().flatten() {
            // `if d.get("is_interruption"): continue` — Python truthiness over
            // whatever the key holds, absent included.
            if detail.get("is_interruption").is_some_and(py_truthy) {
                continue;
            }
            // `(d.get("user_message") or "").strip()`.
            let text = detail
                .get("user_message")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let text = py_strip(text);
            // NARROWING: Python calls `.lower()` on each element and would raise
            // `AttributeError` on a non-string, taking the whole request with
            // it. The aggregator only ever writes strings here, so a non-string
            // is skipped rather than fabricating a 500 nothing can produce.
            let tool_names: Vec<&str> = detail
                .get("tool_names")
                .and_then(Value::as_array)
                .map(|names| names.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            cmd_counts.incr(&classify_activity(text, &tool_names), 1);
        }
    }

    // `if bash_total:` — zero is falsy, so an export with no Bash calls has an
    // EMPTY shell list rather than a zero-count row.
    let mut shell_list = Vec::new();
    if bash_total != 0 {
        let mut obj = Map::new();
        obj.insert("name".to_owned(), Value::from("Bash"));
        obj.insert("calls".to_owned(), Value::from(bash_total));
        // Hardcoded 100.0 — there is only ever one entry, so the share is
        // trivially the whole of it.
        obj.insert("share_pct".to_owned(), jf(100.0));
        shell_list.push(Value::Object(obj));
    }

    Ok((
        share_pct_list(&cmd_counts),
        share_pct_list(&tool_counts),
        share_pct_list(&mcp_counts),
        shell_list,
    ))
}

/// `_classify_activity(text, tool_names)`.
///
/// Slash-commands win outright; otherwise the tool signals are tried in a fixed
/// order and the first hit names the activity. `text` arrives already
/// `.strip()`ed and Python `.lstrip()`s it *again* — harmless, and reproduced so
/// the two functions stay transliterations of each other.
fn classify_activity(text: &str, tool_names: &[&str]) -> String {
    let stripped = py_lstrip(text);
    if stripped.starts_with('/') {
        // `stripped.split()[0][:60]` — whitespace-split (so the token cannot be
        // empty once we know it starts with `/`), then a CODE-POINT slice.
        let head = stripped.split(is_py_space).next().unwrap_or(stripped);
        return py_char_prefix(head, 60).to_owned();
    }
    let tool_set: HashSet<String> = tool_names.iter().map(|t| t.to_lowercase()).collect();
    if ["edit", "multiedit", "write"]
        .iter()
        .any(|t| tool_set.contains(*t))
    {
        return "coding".to_owned();
    }
    if ["read", "grep", "glob"]
        .iter()
        .any(|t| tool_set.contains(*t))
    {
        return "exploration".to_owned();
    }
    if tool_set.contains("bash") {
        return "shell".to_owned();
    }
    if tool_set.contains("websearch") || tool_set.contains("webfetch") {
        return "research".to_owned();
    }
    "chat".to_owned()
}

/// `_share_pct_list(counts)` — `most_common()` plus a percentage.
fn share_pct_list(counts: &Counter) -> Vec<Value> {
    let total: i64 = counts.entries.iter().map(|(_, n)| *n).sum();
    counts
        .most_common()
        .into_iter()
        .map(|(name, n)| {
            let mut obj = Map::new();
            obj.insert("name".to_owned(), Value::from(name));
            obj.insert("calls".to_owned(), Value::from(n));
            // `round(n / total * 100, 2) if total else 0.0` — a Python `int`
            // total of 0 is falsy, and a NEGATIVE total is not, so the guard is
            // on truthiness rather than on `> 0`.
            #[allow(
                clippy::cast_precision_loss,
                reason = "Python's `/` widens both ints to double first"
            )]
            let share = if total == 0 {
                0.0
            } else {
                round_half_even(n as f64 / total as f64 * 100.0, 2)
            };
            obj.insert("share_pct".to_owned(), jf(share));
            Value::Object(obj)
        })
        .collect()
}

// ── renderers (`reports/render.py`) ──────────────────────────────────────────

/// `render_export_json(payload)` — `json.dumps(payload, indent=2, default=str)`.
///
/// **`dumps_pretty`, not `dumps_http`.** See the module docs: this body is built
/// by the route and handed to a bare `Response`, so starlette's
/// `ensure_ascii=False` never touches it.
#[must_use]
pub fn render_export_json(payload: &Value) -> String {
    stax_memory::pyjson::dumps_pretty(payload)
}

/// `render_export_csv(payload)` — one daily section + one activity section per
/// period.
///
/// The layout, measured against the reference:
///
/// ```text
/// # period: <label>          (omitted entirely when the label is empty)
/// date,provider,project,…
/// <daily rows>
///                            (a bare blank line, always)
/// # activity — <label>       (or `# activity` when the label is empty)
/// activity,calls,share_pct
/// <activity rows>
/// ```
///
/// with a *second* blank line between periods in the multi-period shape.
#[must_use]
pub fn render_export_csv(payload: &Value) -> String {
    let mut writer = CsvWriter::default();
    for (index, (label, period)) in iter_periods(payload).into_iter().enumerate() {
        if index > 0 {
            writer.write_raw("\n");
        }
        // `if label:` — the empty label suppresses the whole header row.
        if !label.is_empty() {
            writer.write_row(&[format!("# period: {label}")]);
        }
        writer.write_row(&DAILY_HEADERS.map(str::to_owned));
        for row in period
            .get("daily")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            writer.write_row(&[
                get_str(row, "date"),
                get_str(row, "provider"),
                get_str(row, "project"),
                // `f"{float(row.get('cost_usd', 0.0)):.6f}"` — six decimals, and
                // Rust's `{:.6}` is CPython's `%.6f` (both round the exact
                // decimal expansion half-to-even; pinned in the tests).
                format!("{:.6}", get_f64(row, "cost_usd")),
                get_i64(row, "calls").to_string(),
                get_i64(row, "sessions").to_string(),
                get_i64(row, "input_tokens").to_string(),
                get_i64(row, "output_tokens").to_string(),
                get_i64(row, "cache_read_tokens").to_string(),
                get_i64(row, "cache_write_tokens").to_string(),
            ]);
        }

        writer.write_raw("\n");
        writer.write_row(&[if label.is_empty() {
            "# activity".to_owned()
        } else {
            // An EM DASH, and the CSV goes out as UTF-8 — the byte the
            // `text/csv; charset=utf-8` content type is there for.
            format!("# activity — {label}")
        }]);
        writer.write_row(&ACTIVITY_HEADERS.map(str::to_owned));
        for row in period
            .get("activities")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            writer.write_row(&[
                get_str(row, "name"),
                get_i64(row, "calls").to_string(),
                format!("{:.2}", get_f64(row, "share_pct")),
            ]);
        }
    }
    writer.buf
}

/// `_iter_periods(payload)` — `(label, period)` pairs.
///
/// The multi-period shape is detected by KEY PRESENCE, not by a schema field, so
/// a single-period payload that happened to carry a `today` key would be
/// misread. It cannot: `build_period_export`'s eleven keys are fixed.
fn iter_periods(payload: &Value) -> Vec<(String, &Value)> {
    const EMPTY: &Value = &Value::Null;
    let has = |key: &str| payload.get(key).is_some();
    if has("today") && has("last_7d") && has("last_30d") {
        return ["today", "last_7d", "last_30d"]
            .iter()
            .map(|key| {
                // `sub = payload.get(key) or {}` then `sub.get("label") or key`
                // — an EMPTY label falls back to the DICT KEY, so an unlabelled
                // 30-day block prints as `last_30d`, not as blank.
                let sub = payload.get(*key).filter(|v| py_truthy(v));
                let label = sub
                    .and_then(|v| v.get("label"))
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .unwrap_or(key);
                (label.to_owned(), sub.unwrap_or(EMPTY))
            })
            .collect();
    }
    let label = payload
        .get("label")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    vec![(label, payload)]
}

/// CPython's `csv.writer(buf, lineterminator="\n")` — the `excel` dialect.
///
/// Not a crate, for the reason `routes/misc.rs` does not use `mime_guess`: a
/// third implementation's idea of "CSV" is a third answer. The quoting rules
/// below were measured against `../StackUnderflow/.venv/bin/python`, and one of
/// them is genuinely surprising — **`\r` forces quoting even though the line
/// terminator is `\n`**, because `_csv.c` tests `c == '\r' || c == '\n'` on top
/// of scanning the terminator string.
#[derive(Debug, Default)]
struct CsvWriter {
    buf: String,
}

impl CsvWriter {
    /// `buf.write(text)` — bypasses the writer, as `render_export_csv` does for
    /// its blank separator lines.
    fn write_raw(&mut self, text: &str) {
        self.buf.push_str(text);
    }

    /// `writer.writerow(fields)`.
    fn write_row(&mut self, fields: &[String]) {
        let start = self.buf.len();
        for (index, field) in fields.iter().enumerate() {
            if index > 0 {
                self.buf.push(',');
            }
            Self::append_field(&mut self.buf, field);
        }
        // The empty-record rule: when a row has fields but rendered to zero
        // characters, CPython appends a quoted empty string — so `writerow([""])`
        // is `""` and not a bare newline, while `writerow([])` is just the
        // terminator and `writerow(["", ""])` is the comma alone.
        if !fields.is_empty() && self.buf.len() == start {
            self.buf.push_str("\"\"");
        }
        self.buf.push('\n');
    }

    fn append_field(out: &mut String, field: &str) {
        let needs_quotes = field
            .chars()
            .any(|c| c == ',' || c == '"' || c == '\r' || c == '\n');
        if !needs_quotes {
            out.push_str(field);
            return;
        }
        out.push('"');
        for c in field.chars() {
            // `doublequote=True`: the quote character is escaped by repeating it.
            if c == '"' {
                out.push('"');
            }
            out.push(c);
        }
        out.push('"');
    }
}

// ── small shared helpers ─────────────────────────────────────────────────────

/// Append the three optional `WHERE` clauses every sweep in this module shares,
/// and return the bound parameters in order.
///
/// `if since:` / `if until:` / `if provider:` are Python truthiness — the EMPTY
/// STRING is falsy, so `?provider=` filters nothing. That is not a rounding of
/// the behaviour, it *is* the behaviour.
fn push_scope_filters(
    sql: &mut String,
    since: Option<&str>,
    until: Option<&str>,
    provider: Option<&str>,
) -> Vec<String> {
    let mut params = Vec::new();
    if let Some(since) = since.filter(|s| !s.is_empty()) {
        sql.push_str("AND messages.timestamp >= ? ");
        params.push(since.to_owned());
    }
    if let Some(until) = until.filter(|s| !s.is_empty()) {
        sql.push_str("AND messages.timestamp < ? ");
        params.push(until.to_owned());
    }
    if let Some(provider) = provider.filter(|s| !s.is_empty()) {
        sql.push_str("AND projects.provider = ? ");
        params.push(provider.to_owned());
    }
    params
}

/// `set(xs) if xs else None` — an EMPTY list is `None`, not an empty set, and
/// the difference is total: an empty set would exclude every project.
fn as_set(values: Option<&[String]>) -> Option<HashSet<String>> {
    values
        .filter(|v| !v.is_empty())
        .map(|v| v.iter().cloned().collect())
}

/// `compute_cost(tokens, model, provider=prov or "anthropic", speed=speed)["total_cost"]`.
///
/// Python wraps this in `try/except Exception: cost = 0.0`. The Rust
/// `compute_cost` is infallible, so that arm has no counterpart — recorded in
/// the ledger rather than faked with a `catch_unwind`.
#[allow(
    clippy::too_many_arguments,
    reason = "mirrors compute_cost's own argument list"
)]
fn price(
    engine: &PricingEngine,
    input: i64,
    output: i64,
    cache_read: i64,
    cache_creation: i64,
    model: &str,
    provider: &str,
    speed: &str,
) -> f64 {
    // `provider=prov or "anthropic"` — the empty provider string falls back.
    let provider = if provider.is_empty() {
        "anthropic"
    } else {
        provider
    };
    let tokens = RawTokens::canonical(input, output, cache_creation, cache_read);
    engine
        .compute_cost(&tokens, model, provider, speed, None)
        .total_cost
}

/// `x` or `None` for a string that may be absent — `None` renders as `null`.
fn opt_str(value: Option<&str>) -> Value {
    value.map_or(Value::Null, Value::from)
}

/// The same, for the `filters.include` / `filters.exclude` lists.
fn opt_list(values: Option<&[String]>) -> Value {
    // NOTE: unlike `as_set`, this echoes the list the CALLER passed, empty or
    // not — `filters` is a record of the request, not of the applied filter.
    values.map_or(Value::Null, |values| {
        Value::Array(values.iter().map(|v| Value::from(v.clone())).collect())
    })
}

/// A Python `float` as JSON — `0.0` stays `0.0` and never collapses to `0`.
fn jf(value: f64) -> Value {
    serde_json::Number::from_f64(value).map_or(Value::Null, Value::Number)
}

/// `row.get(key, "")` for a string column of a payload row.
fn get_str(row: &Value, key: &str) -> String {
    row.get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

/// `int(row.get(key, 0))`.
fn get_i64(row: &Value, key: &str) -> i64 {
    row.get(key).and_then(Value::as_i64).unwrap_or(0)
}

/// `float(row.get(key, 0.0))`.
fn get_f64(row: &Value, key: &str) -> f64 {
    row.get(key).and_then(Value::as_f64).unwrap_or(0.0)
}

/// An insertion-ordered map — `dict` with `setdefault`.
///
/// Exists because two of this module's outputs are ordered by insertion
/// (`project_map`'s ties, and the whole `models` dict) and a `HashMap` would
/// make them vary per run.
#[derive(Debug)]
struct Ordered<K, V> {
    index: HashMap<K, usize>,
    keys: Vec<K>,
    values: Vec<V>,
}

impl<K, V> Default for Ordered<K, V> {
    fn default() -> Self {
        Self {
            index: HashMap::new(),
            keys: Vec::new(),
            values: Vec::new(),
        }
    }
}

impl<K: std::hash::Hash + Eq + Clone, V> Ordered<K, V> {
    /// `d.setdefault(key, default())` — a mutable reference either way.
    fn entry(&mut self, key: K, default: impl FnOnce() -> V) -> &mut V {
        if let Some(&i) = self.index.get(&key) {
            return &mut self.values[i];
        }
        self.index.insert(key.clone(), self.values.len());
        self.keys.push(key);
        self.values.push(default());
        self.values
            .last_mut()
            .expect("just pushed, so the vector is non-empty")
    }

    /// `d[key]` where a miss is `None` — the `if key in daily_map` test.
    fn get_mut(&mut self, key: &K) -> Option<&mut V> {
        self.index.get(key).map(|&i| &mut self.values[i])
    }

    /// `d.items()`, mutably.
    fn iter_mut(&mut self) -> impl Iterator<Item = (&K, &mut V)> {
        self.keys.iter().zip(self.values.iter_mut())
    }

    /// `list(d.values())`.
    fn into_values(self) -> Vec<V> {
        self.values
    }

    /// `list(d.items())`.
    fn into_pairs(self) -> Vec<(K, V)> {
        self.keys.into_iter().zip(self.values).collect()
    }
}

/// `collections.Counter[str]` — insertion-ordered, with `most_common()`.
#[derive(Debug, Default)]
struct Counter {
    entries: Vec<(String, i64)>,
    index: HashMap<String, usize>,
}

impl Counter {
    /// `counter[name] += n`, creating the key at zero on first touch.
    fn incr(&mut self, name: &str, n: i64) {
        match self.index.get(name) {
            Some(&i) => self.entries[i].1 += n,
            None => {
                self.index.insert(name.to_owned(), self.entries.len());
                self.entries.push((name.to_owned(), n));
            }
        }
    }

    /// `Counter.most_common()` — `sorted(items, key=count, reverse=True)`.
    ///
    /// CPython's `sorted` is stable and `reverse=True` does **not** reverse
    /// equal elements, so ties come out in FIRST-SEEN order. `sort_by_key` with
    /// a `Reverse` key is stable in the same direction; sorting ascending and
    /// then calling `.reverse()` would flip every tie and is the natural wrong
    /// answer here.
    fn most_common(&self) -> Vec<(String, i64)> {
        let mut out = self.entries.clone();
        out.sort_by_key(|entry| std::cmp::Reverse(entry.1));
        out
    }
}

// ── safe_write_text (`export.py:702`) ────────────────────────────────────────

/// What [`safe_write_text`] refuses to do.
///
/// Python raises `FileExistsError` for all three refusals and lets every other
/// `OSError` propagate; `cli.py`'s `export` catches the first by type and turns
/// it into a `ClickException`, so the split has to survive into Rust or a
/// permission error would print as a friendly message instead of a traceback.
#[derive(Debug)]
pub enum WriteError {
    /// `raise FileExistsError(...)` — the three refusals, message included.
    Exists(String),
    /// Anything else `open`/`replace` raised. Python would traceback here.
    Io(std::io::Error),
}

impl std::fmt::Display for WriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Exists(message) => f.write_str(message),
            Self::Io(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for WriteError {}

/// `safe_write_text(path, content, force=…)` — atomic, symlink-safe.
///
/// Three refusals, each a `FileExistsError` with its own sentence: the target
/// is a symlink, the target exists without `--force`, or the temp path is a
/// symlink. Then `parent.mkdir(parents=True, exist_ok=True)`, a write to
/// `<path>.tmp`, and `Path.replace` — and on ANY exception the temp file is
/// unlinked before the error is re-raised.
///
/// # `p.with_suffix(p.suffix + ".tmp")` is `path + ".tmp"`
///
/// It reads like suffix surgery and it is not: `with_suffix` replaces the final
/// suffix, and the replacement here is *that same suffix* with `.tmp` glued on,
/// so `stem + suffix + ".tmp"` — which is the whole name plus four characters.
/// `out.csv` → `out.csv.tmp`, `out` → `out.tmp`, `a.tar.gz` → `a.tar.gz.tmp`,
/// `.hidden` → `.hidden.tmp` (a leading dot is not a suffix to `pathlib`).
/// Written as an append with the reasoning recorded, rather than as a
/// transcription of the surgery that would have to be re-derived.
///
/// # Errors
/// [`WriteError::Exists`] for the three refusals, [`WriteError::Io`] otherwise.
pub fn safe_write_text(path: &Path, content: &str, force: bool) -> Result<(), WriteError> {
    // `Path("")` is `PosixPath(".")` — `symlink_metadata` on the empty path
    // errors where CPython would inspect the cwd, so normalise first and the
    // `-o ''` row lands on the same "already exists" sentence.
    let target: &Path = if path.as_os_str().is_empty() {
        Path::new(".")
    } else {
        path
    };

    // `p.is_symlink()` — TRUE for a dangling symlink too, which is the point:
    // the refusal is about the link, not about what it points at.
    if is_symlink(target) {
        return Err(WriteError::Exists(format!(
            "Refusing to write through symlink: {}",
            target.display()
        )));
    }
    // `p.exists()` FOLLOWS symlinks; we already returned for those.
    if target.exists() && !force {
        return Err(WriteError::Exists(format!(
            "{} already exists. Pass --force to overwrite.",
            target.display()
        )));
    }

    // `p.parent.mkdir(parents=True, exist_ok=True)`. `Path(".").parent` is
    // `PosixPath(".")`, which already exists, so this is a no-op there.
    let parent = target.parent().unwrap_or(Path::new("."));
    let parent = if parent.as_os_str().is_empty() {
        Path::new(".")
    } else {
        parent
    };
    std::fs::create_dir_all(parent).map_err(WriteError::Io)?;

    let mut tmp_name = target.as_os_str().to_owned();
    tmp_name.push(".tmp");
    let tmp = PathBuf::from(tmp_name);
    if is_symlink(&tmp) {
        return Err(WriteError::Exists(format!(
            "Refusing to write through symlink temp: {}",
            tmp.display()
        )));
    }

    // `open(tmp, "w", encoding="utf-8", newline="")` — no newline translation
    // on any platform, so the bytes are the string's. Mode is the umask's on
    // both sides (`open` is 0666 & ~umask, and so is `fs::write`), so DIV-264's
    // 0600 hazard does not reach this writer.
    let outcome = std::fs::write(&tmp, content.as_bytes())
        .and_then(|()| std::fs::rename(&tmp, target))
        .map_err(WriteError::Io);
    if outcome.is_err() {
        // `except Exception: if tmp.exists(): tmp.unlink()` — best effort, and
        // an `OSError` from the unlink is swallowed.
        let _ = std::fs::remove_file(&tmp);
    }
    outcome
}

/// `Path.is_symlink()` — false for a path that cannot be stat-ed at all.
fn is_symlink(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_symlink())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-07-31T12:34:56.789012+00:00 — the same instant `scope.rs` pins on.
    fn pinned() -> Instant {
        Instant::from_parts(2026, 7, 31, 12, 34, 56, 789_012)
    }

    fn engine() -> PricingEngine {
        stax_etl::stats::dataset::default_engine().expect("the checked-in models.toml parses")
    }

    // ── the JSON writer choice ───────────────────────────────────────────────

    #[test]
    fn the_json_body_is_the_cli_writer_and_escapes_non_ascii() {
        // The whole point of this endpoint being different: `render_export_json`
        // is `json.dumps(..., indent=2)`, i.e. ensure_ascii=True. Measured
        // against the reference — `/api/projects` would ship the raw UTF-8.
        let payload = serde_json::json!({"project": "café…"});
        assert_eq!(
            render_export_json(&payload),
            "{\n  \"project\": \"caf\\u00e9\\u2026\"\n}"
        );
        assert_eq!(
            stax_memory::pyjson::dumps_http(&payload),
            "{\"project\":\"café…\"}"
        );
    }

    #[test]
    fn the_json_body_keeps_indent_two_and_collapses_empty_containers() {
        let payload = serde_json::json!({
            "label": "", "since": null, "until": null,
            "totals": {}, "daily": [], "projects": [], "models": {},
            "activities": [], "tools": [], "mcp": [], "shell": [],
        });
        // Byte-for-byte the reference's `render_export_json` on this payload.
        assert_eq!(
            render_export_json(&payload),
            "{\n  \"label\": \"\",\n  \"since\": null,\n  \"until\": null,\n  \"totals\": {},\n  \
             \"daily\": [],\n  \"projects\": [],\n  \"models\": {},\n  \"activities\": [],\n  \
             \"tools\": [],\n  \"mcp\": [],\n  \"shell\": []\n}"
        );
    }

    // ── the CSV renderer, pinned to measured reference output ────────────────

    #[test]
    fn the_single_period_csv_is_byte_identical_to_the_reference() {
        // Expectation produced by running `render_export_csv` on this exact
        // payload under `../StackUnderflow/.venv/bin/python`. It carries every
        // quoting trigger at once: a comma, an embedded quote, a newline, and a
        // non-ASCII project name that must NOT be quoted or escaped.
        let payload = serde_json::json!({
            "label": "all time",
            "daily": [
                {"date": "2026-07-30", "provider": "claude", "project": "-a-b",
                 "cost_usd": 1.234_567_5, "calls": 3, "sessions": 2, "input_tokens": 10,
                 "output_tokens": 20, "cache_read_tokens": 0, "cache_write_tokens": 0},
                {"date": "2026-07-31", "provider": "", "project": "quote\"and,comma",
                 "cost_usd": 0.0, "calls": 0, "sessions": 0, "input_tokens": 0,
                 "output_tokens": 0, "cache_read_tokens": 0, "cache_write_tokens": 0},
                {"date": "2026-07-31", "provider": "x\ny", "project": "café…",
                 "cost_usd": 1e-7, "calls": 1, "sessions": 1, "input_tokens": 1,
                 "output_tokens": 1, "cache_read_tokens": 1, "cache_write_tokens": 1},
            ],
            "activities": [
                {"name": "/init", "calls": 3, "share_pct": 75.0},
                {"name": "chat,x", "calls": 1, "share_pct": 25.0},
            ],
        });
        assert_eq!(
            render_export_csv(&payload),
            "# period: all time\n\
             date,provider,project,cost_usd,calls,sessions,input_tokens,output_tokens,cache_read_tokens,cache_write_tokens\n\
             2026-07-30,claude,-a-b,1.234568,3,2,10,20,0,0\n\
             2026-07-31,,\"quote\"\"and,comma\",0.000000,0,0,0,0,0,0\n\
             2026-07-31,\"x\ny\",café…,0.000000,1,1,1,1,1,1\n\
             \n\
             # activity — all time\n\
             activity,calls,share_pct\n\
             /init,3,75.00\n\
             \"chat,x\",1,25.00\n"
        );
    }

    #[test]
    fn an_empty_label_drops_the_period_header_and_unlabels_the_activity_one() {
        // `if label:` and `f"# activity — {label}" if label else "# activity"`.
        // An empty database still produces a parseable file with both headers.
        let payload = serde_json::json!({"label": "", "daily": [], "activities": []});
        assert_eq!(
            render_export_csv(&payload),
            "date,provider,project,cost_usd,calls,sessions,input_tokens,output_tokens,cache_read_tokens,cache_write_tokens\n\
             \n\
             # activity\n\
             activity,calls,share_pct\n"
        );
    }

    #[test]
    fn a_multi_period_csv_falls_back_to_the_dict_key_when_a_label_is_empty() {
        // `sub.get("label") or key` — `last_30d` here has no label, so its
        // section header prints the KEY. A reader would predict a blank.
        let payload = serde_json::json!({
            "schema": "stackunderflow.export.v1",
            "today": {"label": "today", "daily": [], "activities": []},
            "last_7d": {"label": "last 7 days", "daily": [
                {"date": "2026-07-25", "provider": "codex", "project": "p", "cost_usd": 0.5,
                 "calls": 2, "sessions": 1, "input_tokens": 5, "output_tokens": 6,
                 "cache_read_tokens": 7, "cache_write_tokens": 8}],
             "activities": [{"name": "coding", "calls": 2, "share_pct": 100.0}]},
            "last_30d": {"label": "", "daily": [], "activities": []},
        });
        let csv = render_export_csv(&payload);
        assert!(csv.starts_with("# period: today\n"), "{csv}");
        assert!(csv.contains("\n# period: last 7 days\n"), "{csv}");
        assert!(csv.contains("\n# period: last_30d\n"), "{csv}");
        assert!(csv.contains("\n# activity — last_30d\n"), "{csv}");
        assert!(
            csv.contains("2026-07-25,codex,p,0.500000,2,1,5,6,7,8\n"),
            "{csv}"
        );
        // A blank line separates period blocks — one before each activity
        // section, plus one before every period after the first.
        assert!(
            csv.contains("activity,calls,share_pct\n\n# period:"),
            "{csv}"
        );
    }

    #[test]
    fn the_csv_quoting_rules_are_cpythons_and_include_the_carriage_return() {
        let mut writer = CsvWriter::default();
        writer.write_row(&["plain".to_owned()]);
        writer.write_row(&["a,b".to_owned()]);
        writer.write_row(&["a\"b".to_owned()]);
        writer.write_row(&["a\nb".to_owned()]);
        // The surprise: `lineterminator` is "\n", yet `\r` still forces quotes.
        writer.write_row(&["a\rb".to_owned()]);
        // Leading whitespace does NOT trigger quoting in the excel dialect.
        writer.write_row(&[" lead".to_owned()]);
        // A single empty field is `""`; two empty fields are just the comma.
        writer.write_row(&[String::new()]);
        writer.write_row(&[String::new(), String::new()]);
        writer.write_row(&[]);
        assert_eq!(
            writer.buf,
            "plain\n\"a,b\"\n\"a\"\"b\"\n\"a\nb\"\n\"a\rb\"\n lead\n\"\"\n,\n\n"
        );
    }

    #[test]
    fn fixed_point_formatting_agrees_with_cpythons_percent_f() {
        // Every expectation here is `"%.6f" % v` / `"%.2f" % v` on the reference
        // interpreter. The two exact ties are the point: both runtimes round the
        // *exact* decimal expansion half-to-even, so 0.125 goes down and 0.375
        // goes up. Arithmetic through `f64::round` would get both wrong.
        assert_eq!(format!("{:.2}", 0.125_f64), "0.12");
        assert_eq!(format!("{:.2}", 0.375_f64), "0.38");
        assert_eq!(format!("{:.2}", 2.675_f64), "2.67");
        assert_eq!(format!("{:.2}", 99.995_f64), "100.00");
        assert_eq!(format!("{:.6}", 1.234_567_5_f64), "1.234568");
        assert_eq!(format!("{:.6}", 1e-7_f64), "0.000000");
        assert_eq!(format!("{:.6}", -0.0_f64), "-0.000000");
    }

    // ── the activity classifier ──────────────────────────────────────────────

    #[test]
    fn a_slash_command_wins_over_every_tool_signal() {
        assert_eq!(classify_activity("/init now", &["Edit"]), "/init");
        // The token is whitespace-split, so a trailing argument is dropped.
        assert_eq!(classify_activity("/plan the thing", &[]), "/plan");
        // …and truncated at 60 CODE POINTS, not bytes.
        let long = format!("/{}", "é".repeat(80));
        assert_eq!(classify_activity(&long, &[]).chars().count(), 60);
    }

    #[test]
    fn the_tool_signals_are_tried_in_a_fixed_order_and_first_hit_wins() {
        // Both a write tool and a read tool: `coding` is checked first.
        assert_eq!(classify_activity("do it", &["Read", "Write"]), "coding");
        assert_eq!(classify_activity("do it", &["Grep"]), "exploration");
        assert_eq!(classify_activity("do it", &["Bash"]), "shell");
        assert_eq!(classify_activity("do it", &["WebFetch"]), "research");
        assert_eq!(classify_activity("do it", &["TodoWrite"]), "chat");
        assert_eq!(classify_activity("", &[]), "chat");
        // Case-insensitive: `{t.lower() for t in tool_names}`.
        assert_eq!(classify_activity("x", &["mUlTiEdIt"]), "coding");
    }

    #[test]
    fn the_lstrip_uses_pythons_whitespace_set_not_unicodes() {
        // U+001F UNIT SEPARATOR is whitespace to Python and not to
        // `str::trim_start`, so a message the store carries with a leading
        // separator still classifies as its slash-command.
        assert_eq!(classify_activity("\u{1f}\u{1c} /init", &[]), "/init");
    }

    // ── share_pct / most_common ──────────────────────────────────────────────

    #[test]
    fn most_common_breaks_ties_in_first_seen_order() {
        let mut counts = Counter::default();
        counts.incr("beta", 2);
        counts.incr("alpha", 2);
        counts.incr("gamma", 5);
        // `sorted(..., reverse=True)` is stable: gamma first, then beta before
        // alpha because beta was seen first. A sort-then-reverse would flip them.
        assert_eq!(
            stax_memory::pyjson::dumps_http(&Value::Array(share_pct_list(&counts))),
            r#"[{"name":"gamma","calls":5,"share_pct":55.56},{"name":"beta","calls":2,"share_pct":22.22},{"name":"alpha","calls":2,"share_pct":22.22}]"#
        );
    }

    #[test]
    fn a_zero_total_gives_a_float_zero_share_and_not_a_division() {
        let mut counts = Counter::default();
        counts.incr("only", 0);
        // `0.0`, not `0` — the int/float split is visible in the bytes (DIV-057).
        assert_eq!(
            stax_memory::pyjson::dumps_http(&Value::Array(share_pct_list(&counts))),
            r#"[{"name":"only","calls":0,"share_pct":0.0}]"#
        );
    }

    // ── totals ───────────────────────────────────────────────────────────────

    #[test]
    fn empty_totals_are_a_float_cost_and_seven_int_zeros() {
        assert_eq!(
            stax_memory::pyjson::dumps_http(&totals_from_daily(&[])),
            r#"{"cost_usd":0.0,"calls":0,"sessions":0,"input_tokens":0,"output_tokens":0,"cache_read_tokens":0,"cache_write_tokens":0,"projects":0}"#
        );
    }

    #[test]
    fn totals_double_count_a_session_that_spans_two_days() {
        // The documented approximation: `sessions` is a SUM of per-day distinct
        // counts. One session active on two days reports 2, and `projects` — a
        // set over `(provider, project)` — still reports 1.
        let daily = vec![
            DailyRow {
                date: "2026-07-30".to_owned(),
                provider: "claude".to_owned(),
                project: "p".to_owned(),
                cost_usd: 0.1,
                calls: 1,
                sessions: 1,
                ..DailyRow::default()
            },
            DailyRow {
                date: "2026-07-31".to_owned(),
                provider: "claude".to_owned(),
                project: "p".to_owned(),
                cost_usd: 0.2,
                calls: 2,
                sessions: 1,
                ..DailyRow::default()
            },
        ];
        assert_eq!(
            stax_memory::pyjson::dumps_http(&totals_from_daily(&daily)),
            r#"{"cost_usd":0.3,"calls":3,"sessions":2,"input_tokens":0,"output_tokens":0,"cache_read_tokens":0,"cache_write_tokens":0,"projects":1}"#
        );
    }

    #[test]
    fn the_cost_total_is_summed_first_and_rounded_second() {
        // 0.1 + 0.2 is the canonical float trap; `round(x, 6)` hides it here,
        // which is exactly why the rounding must come AFTER the sum.
        let daily: Vec<DailyRow> = [0.1_f64, 0.2, 0.000_000_5]
            .iter()
            .map(|c| DailyRow {
                cost_usd: *c,
                ..DailyRow::default()
            })
            .collect();
        let totals = totals_from_daily(&daily);
        // Measured on the reference: `sum([0.1, 0.2, 5e-7])` is exactly
        // `0.3000005` and `round(…, 6)` is `0.300001` — the double sits a hair
        // ABOVE the decimal midpoint, so this is not a tie and does not round
        // to even. Rounding each addend to six places first would have given
        // `0.3` instead, which is why the order matters.
        assert_eq!(totals["cost_usd"], Value::from(0.300_001));
    }

    // ── period plumbing ──────────────────────────────────────────────────────

    #[test]
    fn the_period_map_is_the_cli_vocabulary_not_the_scope_one() {
        assert_eq!(export_period_spec("today"), Some("today"));
        // `week` is NOT a scope spec — it maps to `7days`, and passing `week`
        // straight through to `parse_period` would be a ValueError.
        assert_eq!(export_period_spec("week"), Some("7days"));
        assert_eq!(export_period_spec("month"), Some("30days"));
        assert_eq!(export_period_spec("all"), Some("all"));
        assert_eq!(export_period_spec("7days"), None);
    }

    #[test]
    fn an_unknown_period_is_the_value_error_message_verbatim() {
        let conn = empty_store();
        let err = build_export_payload(
            &conn,
            &engine(),
            Some("fortnight"),
            None,
            None,
            None,
            false,
            &pinned,
        )
        .expect_err("unknown period");
        match err {
            ExportError::Value(msg) => assert_eq!(
                msg,
                "Unknown period 'fortnight'. Valid: all, month, today, week"
            ),
            ExportError::Internal(msg) => panic!("wrong variant: {msg}"),
        }
    }

    #[test]
    fn an_unknown_format_is_the_other_value_error_verbatim() {
        let err = render_export_payload(&Value::Null, "xml").expect_err("unknown format");
        match err {
            ExportError::Value(msg) => assert_eq!(msg, "Unknown format 'xml'. Valid: csv, json"),
            ExportError::Internal(msg) => panic!("wrong variant: {msg}"),
        }
    }

    #[test]
    fn the_filename_embeds_the_period_and_the_utc_day_and_rollup_when_unset() {
        let conn = empty_store();
        let export = run_export(
            &conn,
            &engine(),
            "csv",
            Some("all"),
            None,
            None,
            None,
            &pinned,
        )
        .expect("all-time export");
        assert_eq!(export.filename, "stackunderflow-export-all-2026-07-31.csv");
        assert_eq!(export.content_type, "text/csv");

        let export = run_export(&conn, &engine(), "json", None, None, None, None, &pinned)
            .expect("rollup export");
        // `label = period or "rollup"`.
        assert_eq!(
            export.filename,
            "stackunderflow-export-rollup-2026-07-31.json"
        );
        assert_eq!(export.content_type, "application/json");
    }

    // ── end-to-end over a real (tiny) store ──────────────────────────────────

    /// The three tables every sweep in this module joins, and nothing else.
    fn empty_store() -> Connection {
        let conn = Connection::open_in_memory().expect("in-memory");
        conn.execute_batch(
            "CREATE TABLE projects (
                 id INTEGER PRIMARY KEY, slug TEXT NOT NULL, provider TEXT);
             CREATE TABLE sessions (
                 id INTEGER PRIMARY KEY, project_id INTEGER NOT NULL,
                 session_id TEXT NOT NULL);
             CREATE TABLE messages (
                 id INTEGER PRIMARY KEY, session_fk INTEGER NOT NULL,
                 seq INTEGER NOT NULL DEFAULT 0,
                 timestamp TEXT NOT NULL, role TEXT NOT NULL DEFAULT 'assistant',
                 model TEXT, speed TEXT NOT NULL DEFAULT 'standard',
                 input_tokens INTEGER NOT NULL DEFAULT 0,
                 output_tokens INTEGER NOT NULL DEFAULT 0,
                 cache_read_tokens INTEGER NOT NULL DEFAULT 0,
                 cache_create_tokens INTEGER NOT NULL DEFAULT 0,
                 raw_json TEXT NOT NULL DEFAULT '{}');",
        )
        .expect("schema");
        conn
    }

    fn seeded_store() -> Connection {
        let conn = empty_store();
        conn.execute_batch(
            "INSERT INTO projects (id, slug, provider) VALUES
                 (1, '-a-one', 'claude'), (2, '-b-two', 'codex');
             INSERT INTO sessions (id, project_id, session_id) VALUES
                 (1, 1, 's-1'), (2, 1, 's-2'), (3, 2, 's-3');
             INSERT INTO messages
                 (session_fk, seq, timestamp, model, input_tokens, output_tokens)
             VALUES
                 (1, 0, '2026-07-30T01:00:00+00:00', 'claude-opus-4-5-20251101', 100, 10),
                 (1, 1, '2026-07-30T02:00:00+00:00', 'claude-opus-4-5-20251101', 200, 20),
                 (2, 0, '2026-07-30T03:00:00+00:00', 'claude-opus-4-5-20251101', 300, 30),
                 (2, 1, '2026-07-31T04:00:00+00:00', '', 400, 40),
                 (3, 0, '2026-07-31T05:00:00+00:00', 'gpt-5-codex', 500, 50);",
        )
        .expect("rows");
        conn
    }

    #[test]
    fn an_all_time_csv_over_a_seeded_store_rolls_days_and_counts_sessions() {
        let conn = seeded_store();
        let export = run_export(
            &conn,
            &engine(),
            "csv",
            Some("all"),
            None,
            None,
            None,
            &pinned,
        )
        .expect("export");
        let lines: Vec<&str> = export.text.lines().collect();
        assert_eq!(lines[0], "# period: all time");
        assert_eq!(lines[1], DAILY_HEADERS.join(","));
        // Three daily buckets: (30th, claude, -a-one) collapses the two sessions
        // and their model rows; the 31st splits by project. Then the blank
        // separator, the activity header and the column row — eight lines, and
        // no activity data because CSV never runs the deep pass.
        assert_eq!(lines.len(), 8, "{lines:?}");
        assert!(
            lines[2].starts_with("2026-07-30,claude,-a-one,"),
            "{lines:?}"
        );
        // `sessions` on the 30th is 2 — from the separate distinct-count sweep,
        // not from the grouped query's per-(model, speed) count.
        assert!(lines[2].ends_with(",3,2,600,60,0,0"), "{}", lines[2]);
        assert!(
            lines[3].starts_with("2026-07-31,claude,-a-one,"),
            "{lines:?}"
        );
        // The empty-model row still appears in `daily`, and costs 0.000000.
        assert!(
            lines[3].contains(",0.000000,1,1,400,40,0,0"),
            "{}",
            lines[3]
        );
        // The 31st also carries the codex project, priced from the manifest.
        assert!(
            lines[4].starts_with("2026-07-31,codex,-b-two,"),
            "{lines:?}"
        );
        assert_eq!(lines[5], "");
        assert_eq!(lines[6], "# activity — all time");
        assert_eq!(lines[7], ACTIVITY_HEADERS.join(","));
    }

    #[test]
    fn the_empty_model_bucket_is_in_daily_and_absent_from_models() {
        let conn = seeded_store();
        let scope = parse_period("all", pinned()).expect("known spec");
        let payload = build_period_export(&conn, &engine(), &scope, None, None, None, false)
            .expect("payload");
        let models = payload["models"].as_object().expect("models object");
        assert!(!models.contains_key(""), "the empty model is dropped");
        assert_eq!(models.len(), 2);
        // `daily` keeps it: the 31st/-a-one row has 400 input tokens and no cost.
        let daily = payload["daily"].as_array().expect("daily array");
        assert_eq!(daily.len(), 3);
        assert!(
            daily
                .iter()
                .any(|r| r["date"] == "2026-07-31" && r["input_tokens"] == 400),
            "{daily:?}"
        );
    }

    #[test]
    fn include_and_exclude_are_slug_filters_applied_after_the_sql() {
        let conn = seeded_store();
        let scope = parse_period("all", pinned()).expect("known spec");
        let only_a = build_period_export(
            &conn,
            &engine(),
            &scope,
            None,
            Some(&["-a-one".to_owned()]),
            None,
            false,
        )
        .expect("payload");
        assert_eq!(only_a["projects"].as_array().expect("array").len(), 1);
        assert_eq!(only_a["totals"]["projects"], Value::from(1));

        let not_a = build_period_export(
            &conn,
            &engine(),
            &scope,
            None,
            None,
            Some(&["-a-one".to_owned()]),
            false,
        )
        .expect("payload");
        assert_eq!(not_a["projects"][0]["name"], Value::from("-b-two"));

        // An EMPTY include list is `None`, not "match nothing" — `if include:`.
        let empty_include =
            build_period_export(&conn, &engine(), &scope, None, Some(&[]), None, false)
                .expect("payload");
        assert_eq!(
            empty_include["projects"].as_array().expect("array").len(),
            2
        );
    }

    #[test]
    fn an_empty_provider_string_filters_nothing() {
        // `if provider:` — `?provider=` is falsy and must NOT become
        // `WHERE projects.provider = ''`, which would empty the export.
        let conn = seeded_store();
        let scope = parse_period("all", pinned()).expect("known spec");
        let payload = build_period_export(&conn, &engine(), &scope, Some(""), None, None, false)
            .expect("payload");
        assert_eq!(payload["projects"].as_array().expect("array").len(), 2);
        let filtered =
            build_period_export(&conn, &engine(), &scope, Some("codex"), None, None, false)
                .expect("payload");
        assert_eq!(filtered["projects"].as_array().expect("array").len(), 1);
    }

    #[test]
    fn projects_sort_by_cost_descending_and_count_sessions_distinctly() {
        let conn = seeded_store();
        let scope = parse_period("all", pinned()).expect("known spec");
        let payload = build_period_export(&conn, &engine(), &scope, None, None, None, false)
            .expect("payload");
        let projects = payload["projects"].as_array().expect("array");
        let costs: Vec<f64> = projects
            .iter()
            .map(|p| p["cost_usd"].as_f64().unwrap_or(0.0))
            .collect();
        assert!(costs[0] >= costs[1], "{costs:?}");
        // Per-project sessions are TRUE distinct counts (2 for -a-one), unlike
        // the totals block's per-day sum.
        let a = projects
            .iter()
            .find(|p| p["name"] == "-a-one")
            .expect("-a-one present");
        assert_eq!(a["sessions"], Value::from(2));
    }

    #[test]
    fn the_multi_period_rollup_shares_one_clock_across_all_three_windows() {
        let conn = empty_store();
        let payload =
            build_multi_period_export(&conn, &engine(), None, None, None, false, pinned())
                .expect("payload");
        assert_eq!(payload["schema"], Value::from("stackunderflow.export.v1"));
        assert_eq!(
            payload["generated"],
            Value::from("2026-07-31T12:34:56.789012+00:00")
        );
        // One clock read: the 7-day and 30-day windows END at exactly the
        // instant `generated` names, to the microsecond.
        assert_eq!(
            payload["last_7d"]["until"],
            Value::from("2026-07-31T12:34:56.789012+00:00")
        );
        assert_eq!(
            payload["last_30d"]["until"],
            Value::from("2026-07-31T12:34:56.789012+00:00")
        );
        assert_eq!(
            payload["last_7d"]["since"],
            Value::from("2026-07-24T12:34:56.789012+00:00")
        );
        // `today` zeroes the microsecond, so it does NOT carry the fraction.
        assert_eq!(
            payload["today"]["since"],
            Value::from("2026-07-31T00:00:00+00:00")
        );
        // The filters block echoes the request, nulls included.
        assert_eq!(
            stax_memory::pyjson::dumps_http(&payload["filters"]),
            r#"{"provider":null,"include":null,"exclude":null}"#
        );
    }

    #[test]
    fn an_empty_store_still_renders_a_parseable_csv_with_headers() {
        let conn = empty_store();
        let export = run_export(
            &conn,
            &engine(),
            "csv",
            Some("today"),
            None,
            None,
            None,
            &pinned,
        )
        .expect("export");
        assert_eq!(
            export.text,
            "# period: today\n\
             date,provider,project,cost_usd,calls,sessions,input_tokens,output_tokens,cache_read_tokens,cache_write_tokens\n\
             \n\
             # activity — today\n\
             activity,calls,share_pct\n"
        );
    }

    #[test]
    fn the_json_export_key_order_is_the_literals() {
        let conn = empty_store();
        let export = run_export(
            &conn,
            &engine(),
            "json",
            Some("all"),
            None,
            None,
            None,
            &pinned,
        )
        .expect("export");
        assert_eq!(
            export.text,
            "{\n  \"label\": \"all time\",\n  \"since\": null,\n  \"until\": null,\n  \
             \"totals\": {\n    \"cost_usd\": 0.0,\n    \"calls\": 0,\n    \"sessions\": 0,\n    \
             \"input_tokens\": 0,\n    \"output_tokens\": 0,\n    \"cache_read_tokens\": 0,\n    \
             \"cache_write_tokens\": 0,\n    \"projects\": 0\n  },\n  \"daily\": [],\n  \
             \"projects\": [],\n  \"models\": {},\n  \"activities\": [],\n  \"tools\": [],\n  \
             \"mcp\": [],\n  \"shell\": []\n}"
        );
    }

    #[test]
    fn the_deep_pass_runs_only_for_json_and_produces_the_four_sections() {
        // The seeded store's `raw_json` is `{}` for every row, so the pipeline
        // yields no tools and no commands — which is the honest assertion here:
        // the four keys exist and are EMPTY, and the CSV path never even looks.
        let conn = seeded_store();
        let scope = parse_period("all", pinned()).expect("known spec");
        let deep =
            build_period_export(&conn, &engine(), &scope, None, None, None, true).expect("payload");
        for key in ["activities", "tools", "mcp", "shell"] {
            assert!(deep[key].is_array(), "{key} is a list");
        }
        let shallow = build_period_export(&conn, &engine(), &scope, None, None, None, false)
            .expect("payload");
        assert_eq!(shallow["activities"], Value::Array(Vec::new()));
    }
}
