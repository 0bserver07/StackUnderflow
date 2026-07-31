//! `services/compare.py` — the per-model comparison table.
//!
//! One window (today / this week / this month / all time), one row per model the
//! user touched in it: how many sessions it was the *primary* model of, how many
//! assistant calls it answered, how often it answered in one shot, how often the
//! assistant had to keep going, what fraction of its cacheable tokens were cache
//! *reads*, and the unit economics ($/call, $/session) on top of the totals.
//!
//! | Item | Python | Rust |
//! |---|---|---|
//! | `PERIOD_MAP` | `dict[str, str]` | [`PERIOD_MAP`], [`period_spec`] |
//! | `ModelStats` | `@dataclass(frozen=True, slots=True)` | [`ModelStats`] |
//! | `compare_models` | dispatches mart vs aggregator | [`compare_models`] |
//! | `_compare_models_from_marts` | `model_day_mart` + `session_mart` | `compare_models_from_marts` |
//! | `_compare_models_from_messages` | the raw-`messages` fallback | `compare_models_from_messages` |
//! | `build_compare_payload` | the HTTP/CLI dict | [`build_compare_payload`] |
//!
//! `routes/compare.py` and the `stackunderflow compare` CLI verb both call
//! `build_compare_payload`, which is why this lives in `services/` rather than in
//! the route module: wave 8 ports the verb and must find the logic already here.
//!
//! # What is load-bearing
//!
//! * **There are two codepaths and they do not compute the same numbers.** When
//!   no `project=` filter is set *and* both marts are materialised, every figure
//!   comes from the marts. Add one `project=` and the whole thing falls back to
//!   a `JOIN` over the raw `messages` view and re-prices every assistant message
//!   through the cost engine. `model_day_mart` carries no `project_id`, which is
//!   the stated reason. So the same window can answer with two different totals
//!   depending on a query parameter, and both are "correct" — this port
//!   reproduces both, including the ways they disagree.
//! * **`period` is aliased twice.** The route's allow-list is
//!   `("today", "week", "month", "all")`; `reports/scope.py` has never heard of
//!   `week`. [`PERIOD_MAP`] is the translation layer, and `week` → `7days` is
//!   the only entry that is not the identity.
//! * **Every ratio has a zero-denominator guard and the fallback is `0.0`, a
//!   float.** `0` and `0.0` render as different bytes (`0` vs `0.0`), so the
//!   guard's *type* is part of the response contract, not a style choice.
//! * **The sort is stable and descending.** `list.sort(key=…, reverse=True)`
//!   keeps the original relative order of equal keys (CPython reverses, sorts
//!   ascending, reverses again). Two models with identical `total_cost`
//!   therefore come out in dict-insertion order — which is SQL row order — and a
//!   comparator that flipped ties would be a byte divergence.
//! * **`sum()` never appears in this module.** Every accumulation is `x += y` or
//!   `d[k] = d.get(k, 0) + v`, so the campaign's compensated-summation rule
//!   (LAW 3) says: plain `+=` here, no Neumaier. The one compensated sum in the
//!   response is SQLite's own `SUM(cost_usd)` over `model_day_mart`, and both
//!   sides run a bundled SQLite ≥ 3.43, where `sum()` is Kahan-Babuska-Neumaier.
//!
//! # The mart-query helpers are duplicated here on purpose
//!
//! `store/mart_queries.py`'s four compare-facing reads (`mart_has_session_rows`,
//! `mart_has_model_day_rows`, `model_day_totals`, `session_mart_rows_for_compare`)
//! are private functions in this file rather than calls into
//! [`super::mart_queries`], which is still an unported stub owned by another
//! member of this batch. The SQL text is verbatim so the integrator can lift them
//! wholesale — recorded as DIV-089.

use std::collections::HashMap;

use anyhow::Result;
use rusqlite::Connection;
use serde_json::{Map, Value};
use stax_etl::pricing::{PricingEngine, RawTokens};

use super::scope::{Instant, Scope, parse_period};

/// `PERIOD_MAP` — CLI/HTTP alias → the spec `reports/scope.parse_period` knows.
///
/// Declaration order is the Python dict's insertion order, which matters for one
/// reason only: `_resolve_scope`'s error message joins `sorted(PERIOD_MAP)`, not
/// the literal order. See [`unknown_period_message`].
pub const PERIOD_MAP: [(&str, &str); 4] = [
    ("today", "today"),
    // The only non-identity entry, and the reason this table exists at all.
    ("week", "7days"),
    ("month", "month"),
    ("all", "all"),
];

/// `PERIOD_MAP.get(period)`.
#[must_use]
pub fn period_spec(period: &str) -> Option<&'static str> {
    PERIOD_MAP
        .iter()
        .find(|(alias, _)| *alias == period)
        .map(|(_, spec)| *spec)
}

/// `_resolve_scope`'s `ValueError` text.
///
/// `', '.join(sorted(PERIOD_MAP))` — **sorted**, so this reads
/// `all, month, today, week`, while `routes/compare.py`'s 400 joins its own
/// tuple and reads `today, week, month, all`. Two different orderings of the same
/// four words in the same feature; both are reproduced where they belong.
#[must_use]
pub fn unknown_period_message(period: &str) -> String {
    let mut aliases: Vec<&str> = PERIOD_MAP.iter().map(|(alias, _)| *alias).collect();
    aliases.sort_unstable();
    format!("Unknown period '{period}'. Valid: {}", aliases.join(", "))
}

/// `_resolve_scope(period)`.
///
/// Unreachable from HTTP — `routes/compare.py` validates against its own
/// allow-list first, and that allow-list is exactly [`PERIOD_MAP`]'s keys. Kept
/// because the CLI verb (wave 8) calls the service directly and *will* reach it.
fn resolve_scope(period: &str, now: Instant) -> Result<Scope> {
    let Some(spec) = period_spec(period) else {
        return Err(anyhow::anyhow!(unknown_period_message(period)));
    };
    // `parse_period` only errors on a spec it does not know; every value in
    // PERIOD_MAP is one it does, so this leg is defence in depth.
    parse_period(spec, now).map_err(|message| anyhow::anyhow!(message))
}

/// `@dataclass(frozen=True, slots=True) class ModelStats` — one row of the table.
///
/// Field order is the dataclass declaration order, because `asdict()` walks
/// `__dataclass_fields__` and the resulting dict is serialised in that order.
/// The int/float split is deliberate and visible in the JSON: `sessions`,
/// `calls` and `total_tokens` render as `3`, the five ratios and the two dollar
/// figures render as `3.0`.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelStats {
    /// The model id, exactly as the store recorded it.
    pub model: String,
    /// The provider that produced the transcript — `"anthropic"` when unknown.
    pub provider: String,
    /// Sessions whose *primary* model this was.
    pub sessions: i64,
    /// Assistant messages attributed to this model.
    pub calls: i64,
    /// One-shot sessions ÷ sessions. A ratio despite the name.
    pub one_shot_pct: f64,
    /// `assistant_messages / sessions - 1.0` — extra answers per session.
    pub retry_rate: f64,
    /// `cache_read / (cache_read + cache_create)`.
    pub cache_hit_rate: f64,
    /// `total_cost / calls`.
    pub cost_per_call: f64,
    /// `total_cost / sessions`.
    pub cost_per_session: f64,
    /// Dollars, summed message-by-message (aggregator path) or read from the
    /// mart (mart path).
    pub total_cost: f64,
    /// input + output + cache-create + cache-read.
    pub total_tokens: i64,
}

impl ModelStats {
    /// `dataclasses.asdict(row)` — the eleven keys, in declaration order.
    #[must_use]
    pub fn to_value(&self) -> Value {
        let mut out = Map::new();
        out.insert("model".to_owned(), Value::String(self.model.clone()));
        out.insert("provider".to_owned(), Value::String(self.provider.clone()));
        out.insert("sessions".to_owned(), Value::from(self.sessions));
        out.insert("calls".to_owned(), Value::from(self.calls));
        out.insert("one_shot_pct".to_owned(), float(self.one_shot_pct));
        out.insert("retry_rate".to_owned(), float(self.retry_rate));
        out.insert("cache_hit_rate".to_owned(), float(self.cache_hit_rate));
        out.insert("cost_per_call".to_owned(), float(self.cost_per_call));
        out.insert("cost_per_session".to_owned(), float(self.cost_per_session));
        out.insert("total_cost".to_owned(), float(self.total_cost));
        out.insert("total_tokens".to_owned(), Value::from(self.total_tokens));
        Value::Object(out)
    }
}

/// A Python `float`, kept a float even when its value is integral.
///
/// `Value::from(0.0_f64)` is already an `f64` node and `pyjson` renders it
/// `0.0`; the wrapper exists so a reader can see, at every call site, that the
/// int/float distinction was a decision.
fn float(value: f64) -> Value {
    // `allow_nan=False` in starlette's writer would raise on a non-finite; the
    // arithmetic here cannot produce one (every division is guarded), so this is
    // a plain conversion with no fallback path to hide a bug behind.
    Value::from(value)
}

/// `compare_models(conn, period=…, project_filter=…, provider_filter=…)`.
///
/// Rows come back sorted by `total_cost` descending, ties in SQL row order.
///
/// `now` is injected rather than read from the clock here — campaign finding 5;
/// it is the instant `parse_period` builds the window around.
///
/// # Errors
/// An unknown `period`, or any SQLite failure.
pub fn compare_models(
    conn: &Connection,
    engine: &PricingEngine,
    period: &str,
    project_filter: Option<&[String]>,
    provider_filter: Option<&str>,
    now: Instant,
) -> Result<Vec<ModelStats>> {
    let scope = resolve_scope(period, now)?;

    // Wave 4A mart fast-path. `project_filter is None` — an EMPTY list is not
    // None and would take the fallback while filtering on nothing, which is a
    // shape HTTP cannot produce (FastAPI gives `None` for an absent repeated
    // parameter) but the CLI could.
    if project_filter.is_none() && mart_has_session_rows(conn)? && mart_has_model_day_rows(conn)? {
        return compare_models_from_marts(conn, &scope, provider_filter);
    }

    compare_models_from_messages(conn, engine, &scope, project_filter, provider_filter)
}

/// `build_compare_payload(...)` — the three-key dict the route returns.
///
/// `generated` is a closure, not an `f64`, because Python evaluates
/// `time.time()` *inside the returned dict literal*, i.e. after the query has
/// run. On a store where the query takes two seconds that is a two-second
/// difference in the field, and the field is the response.
///
/// # Errors
/// See [`compare_models`].
pub fn build_compare_payload(
    conn: &Connection,
    engine: &PricingEngine,
    period: &str,
    project_filter: Option<&[String]>,
    provider_filter: Option<&str>,
    now: Instant,
    generated: impl FnOnce() -> f64,
) -> Result<Value> {
    let rows = compare_models(conn, engine, period, project_filter, provider_filter, now)?;
    let mut out = Map::new();
    // `"period": period` — the INPUT alias is echoed, not the resolved spec, so
    // `?period=week` answers `"period":"week"` and never `"7days"`.
    out.insert("period".to_owned(), Value::String(period.to_owned()));
    out.insert(
        "models".to_owned(),
        Value::Array(rows.iter().map(ModelStats::to_value).collect()),
    );
    out.insert("generated".to_owned(), float(generated()));
    Ok(Value::Object(out))
}

/// `time.time()`.
///
/// CPython does not build this float the way [`std::time::Duration::as_secs_f64`]
/// does. `_PyTime_AsSecondsDouble` takes the clock as an integer nanosecond
/// count and then divides *once*:
///
/// ```text
/// if (t % SEC_TO_NS == 0) { d = (double)(t / SEC_TO_NS); }
/// else                    { d = (double)t; d /= 1e9; }
/// ```
///
/// while `as_secs_f64` is `secs as f64 + nanos as f64 / 1e9` — two roundings
/// against one, so the two disagree in the last bit on most instants. The value
/// is wall-clock noise either way; the *construction* is reproduced because
/// "close enough" is how a float divergence gets shipped.
#[must_use]
pub fn now_unix_seconds() -> f64 {
    let nanos = match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(delta) => i128::from(delta.as_secs()) * 1_000_000_000 + i128::from(delta.subsec_nanos()),
        Err(err) => {
            let delta = err.duration();
            -(i128::from(delta.as_secs()) * 1_000_000_000 + i128::from(delta.subsec_nanos()))
        }
    };
    py_time_as_seconds_double(nanos)
}

/// `_PyTime_AsSecondsDouble` — see [`now_unix_seconds`].
#[allow(
    clippy::cast_precision_loss,
    reason = "the lossy cast IS the CPython behaviour being reproduced"
)]
fn py_time_as_seconds_double(nanos: i128) -> f64 {
    if nanos % 1_000_000_000 == 0 {
        (nanos / 1_000_000_000) as f64
    } else {
        (nanos as f64) / 1e9
    }
}

// ── the aggregator path: raw `messages`, re-priced ───────────────────────────

/// One row of `_fetch_messages`.
struct MessageRow {
    session_fk: i64,
    role: Option<String>,
    model: String,
    input_tokens: i64,
    output_tokens: i64,
    cache_create_tokens: i64,
    cache_read_tokens: i64,
    speed: String,
    provider: Option<String>,
}

/// `_Acc` — the per-model accumulator of pass 3.
#[derive(Default)]
struct Acc {
    provider: String,
    calls: i64,
    total_cost: f64,
    input_tokens: i64,
    output_tokens: i64,
    cache_create_tokens: i64,
    cache_read_tokens: i64,
}

/// `_compare_models_from_messages` — the empty-mart / project-filtered fallback.
///
/// Python makes four passes over one `fetchall()`. Passes 1 and 3 are fused here
/// into the single row loop that reads the cursor: they touch disjoint state
/// (pass 1 fills the per-session counters, pass 3 fills the per-model
/// accumulator), neither reads the other's output, and pass 3's insertion order
/// — which decides the tie order of the final sort — is the row order either
/// way. The win is that the whole result set never has to be materialised, which
/// on the `all` window is every assistant message in the store.
fn compare_models_from_messages(
    conn: &Connection,
    engine: &PricingEngine,
    scope: &Scope,
    project_filter: Option<&[String]>,
    provider_filter: Option<&str>,
) -> Result<Vec<ModelStats>> {
    // Pass 1 state.
    let mut per_session_model_counts: HashMap<i64, HashMap<String, i64>> = HashMap::new();
    let mut per_session_user: HashMap<i64, i64> = HashMap::new();
    let mut per_session_assistant: HashMap<i64, i64> = HashMap::new();
    // Pass 3 state — insertion-ordered, because `by_model` is a plain dict.
    let mut by_model: OrderedMap<Acc> = OrderedMap::default();

    fetch_messages(conn, scope, project_filter, provider_filter, |row| {
        let role = row.role.as_deref();
        if role == Some("user") {
            *per_session_user.entry(row.session_fk).or_insert(0) += 1;
        } else if role == Some("assistant") {
            *per_session_assistant.entry(row.session_fk).or_insert(0) += 1;
            // `mdl = r["model"] or ""` — the column is already COALESCEd, so
            // this second `or` only catches an empty string, which stays empty.
            let bucket = per_session_model_counts.entry(row.session_fk).or_default();
            *bucket.entry(row.model.clone()).or_insert(0) += 1;
        }

        // ── pass 3, fused ───────────────────────────────────────────────────
        if role != Some("assistant") {
            return Ok(());
        }
        if row.model.is_empty() {
            // "Skip rows that never had a model recorded — they would always
            // price at $0 and pollute the comparison table." Note that they
            // still counted toward `per_session_assistant` above, so they DO
            // move the retry rate of whichever model wins the session.
            return Ok(());
        }
        let acc = by_model.entry(&row.model);
        // `acc.provider = acc.provider or (r["provider"] or "")` — first
        // non-empty wins, and it is never revisited once set.
        if acc.provider.is_empty() {
            acc.provider = row.provider.clone().unwrap_or_default();
        }
        acc.calls += 1;
        acc.input_tokens += row.input_tokens;
        acc.output_tokens += row.output_tokens;
        acc.cache_create_tokens += row.cache_create_tokens;
        acc.cache_read_tokens += row.cache_read_tokens;
        // LAW 2: the engine is injected by the route from the store's own
        // `price_book`, never `default_engine()`. The `provider` handed to the
        // pricer is the TOOL that wrote the transcript (`claude`, `codex`, …),
        // passed through verbatim — `get_pricer` falls back to Anthropic for an
        // id it does not know, and `compare.py` relies on that fallback.
        let cost = engine
            .compute_cost(
                &RawTokens::canonical(
                    row.input_tokens,
                    row.output_tokens,
                    row.cache_create_tokens,
                    row.cache_read_tokens,
                ),
                &row.model,
                row.provider
                    .as_deref()
                    .filter(|p| !p.is_empty())
                    .unwrap_or("anthropic"),
                // `speed=r["speed"] or "standard"` — the COALESCE already
                // handles NULL, this handles `''`.
                if row.speed.is_empty() {
                    "standard"
                } else {
                    &row.speed
                },
                None,
            )
            .total_cost;
        // A `+=` chain, not `sum()`. LAW 3: do NOT compensate.
        acc.total_cost += cost;
        Ok(())
    })?;

    // Pass 2: pick a primary model for every session that has assistant messages.
    // Pass 4: fold those sessions into per-model counters. Python keeps them
    // apart because pass 2 builds a dict it never uses elsewhere; merged here.
    let mut sessions_by_model: HashMap<String, i64> = HashMap::new();
    let mut one_shot_by_model: HashMap<String, i64> = HashMap::new();
    let mut assistant_msgs_by_model: HashMap<String, i64> = HashMap::new();
    for (session, counts) in &per_session_model_counts {
        let model = primary_model_for_session(counts);
        if model.is_empty() {
            continue;
        }
        let users = per_session_user.get(session).copied().unwrap_or(0);
        let assistants = per_session_assistant.get(session).copied().unwrap_or(0);
        *sessions_by_model.entry(model.clone()).or_insert(0) += 1;
        *assistant_msgs_by_model.entry(model.clone()).or_insert(0) += assistants;
        // "A session counts as one-shot when there's exactly one user prompt
        // and one assistant reply." Exactly one, not at most one.
        if users == 1 && assistants == 1 {
            *one_shot_by_model.entry(model).or_insert(0) += 1;
        }
    }

    let mut out: Vec<ModelStats> = Vec::new();
    for (model, acc) in by_model.into_pairs() {
        let sessions = sessions_by_model.get(&model).copied().unwrap_or(0);
        let one_shot = one_shot_by_model.get(&model).copied().unwrap_or(0);
        let assistant_msgs = assistant_msgs_by_model.get(&model).copied().unwrap_or(0);
        let cacheable = acc.cache_read_tokens + acc.cache_create_tokens;
        out.push(ModelStats {
            // `provider=acc.provider or "anthropic"`.
            provider: if acc.provider.is_empty() {
                "anthropic".to_owned()
            } else {
                acc.provider.clone()
            },
            model,
            sessions,
            calls: acc.calls,
            one_shot_pct: ratio(one_shot, sessions),
            retry_rate: retry_rate(assistant_msgs, sessions),
            cache_hit_rate: ratio(acc.cache_read_tokens, cacheable),
            // `if acc.calls else 0.0` — a dead guard: an accumulator only exists
            // because a row incremented `calls`, so it is never 0. Ported anyway.
            cost_per_call: per_unit(acc.total_cost, acc.calls),
            cost_per_session: per_unit(acc.total_cost, sessions),
            total_cost: acc.total_cost,
            total_tokens: acc.input_tokens
                + acc.output_tokens
                + acc.cache_create_tokens
                + acc.cache_read_tokens,
        });
    }

    sort_by_total_cost_desc(&mut out);
    Ok(out)
}

/// `_fetch_messages` — the one SQL pass, streamed a row at a time.
///
/// **The `JOIN` is inherited, not chosen.** Spec §6b says a list subquery is the
/// safe shape against the partitioned `messages` VIEW, because a JOIN makes the
/// planner materialise the whole union — that is the July hang. Python writes
/// two JOINs here anyway, and bug-for-bug (LAW 6) means the port writes them
/// too. This codepath only runs when a `project=` filter is present, so the hang
/// risk is real but narrow; recorded as DIV-088 rather than silently improved.
fn fetch_messages(
    conn: &Connection,
    scope: &Scope,
    project_filter: Option<&[String]>,
    provider_filter: Option<&str>,
    mut visit: impl FnMut(&MessageRow) -> Result<()>,
) -> Result<()> {
    let mut sql = String::from(
        "SELECT messages.session_fk AS session_fk, \
                messages.role AS role, \
                COALESCE(messages.model, '') AS model, \
                COALESCE(messages.input_tokens, 0) AS input_tokens, \
                COALESCE(messages.output_tokens, 0) AS output_tokens, \
                COALESCE(messages.cache_create_tokens, 0) AS cache_create_tokens, \
                COALESCE(messages.cache_read_tokens, 0) AS cache_read_tokens, \
                COALESCE(messages.speed, 'standard') AS speed, \
                projects.provider AS provider \
         FROM messages \
         JOIN sessions ON sessions.id = messages.session_fk \
         JOIN projects ON projects.id = sessions.project_id \
         WHERE 1=1 ",
    );
    let mut params: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
    if let Some(since) = &scope.since {
        sql.push_str("AND messages.timestamp >= ? ");
        params.push(Box::new(since.clone()));
    }
    if let Some(until) = &scope.until {
        sql.push_str("AND messages.timestamp <= ? ");
        params.push(Box::new(until.clone()));
    }
    // `if provider_filter:` — truthiness, so `?provider=` (empty) filters on
    // NOTHING here, while the mart path's `is not None` test treats the same
    // empty string as a live filter. DIV-086.
    if let Some(provider) = provider_filter.filter(|value| !value.is_empty()) {
        sql.push_str("AND projects.provider = ? ");
        params.push(Box::new(provider.to_owned()));
    }
    if let Some(projects) = project_filter.filter(|list| !list.is_empty()) {
        let placeholders = vec!["?"; projects.len()].join(",");
        sql.push_str(&format!("AND projects.slug IN ({placeholders}) "));
        for slug in projects {
            params.push(Box::new(slug.clone()));
        }
    }

    let mut stmt = conn.prepare(&sql)?;
    let refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(std::convert::AsRef::as_ref).collect();
    let mut rows = stmt.query(refs.as_slice())?;
    while let Some(row) = rows.next()? {
        // The `JOIN sessions ON sessions.id = messages.session_fk` makes a NULL
        // `session_fk` unmatchable, so the column is never NULL in a row that
        // reaches here.
        let parsed = MessageRow {
            session_fk: row.get(0)?,
            role: row.get(1)?,
            model: row.get(2)?,
            input_tokens: row.get(3)?,
            output_tokens: row.get(4)?,
            cache_create_tokens: row.get(5)?,
            cache_read_tokens: row.get(6)?,
            speed: row.get(7)?,
            provider: row.get(8)?,
        };
        visit(&parsed)?;
    }
    Ok(())
}

/// `_primary_model_for_session(model_counts)`.
///
/// The most-used model, ties broken lexicographically, and the empty string
/// ("no model recorded") losing to any real id even when it has the same count.
fn primary_model_for_session(model_counts: &HashMap<String, i64>) -> String {
    let Some(best_count) = model_counts.values().copied().max() else {
        return String::new();
    };
    let mut candidates: Vec<&str> = model_counts
        .iter()
        .filter(|(_, count)| **count == best_count)
        .map(|(model, _)| model.as_str())
        .collect();
    candidates.sort_unstable();
    // `for m in candidates: if m: return m` — the first NON-EMPTY in sorted
    // order, not the first.
    for model in &candidates {
        if !model.is_empty() {
            return (*model).to_owned();
        }
    }
    // `return candidates[0]` — reachable only when every candidate is "", so
    // the value is "" and the caller's `if not mdl: continue` drops the session.
    candidates
        .first()
        .map_or_else(String::new, |m| (*m).to_owned())
}

// ── the mart path: `model_day_mart` + `session_mart` ─────────────────────────

/// `model_day_totals`' per-model row.
#[derive(Default)]
struct ModelTotals {
    cost_usd: f64,
    input_tokens: i64,
    output_tokens: i64,
    cache_read: i64,
    cache_create: i64,
    message_count: i64,
}

/// `_compare_models_from_marts`.
///
/// Per-model totals come from `model_day_mart`; per-session attribution
/// (sessions, one-shot, retry, provider) from `session_mart`. Note what this
/// means for `cost_per_session`: the numerator counts *every* event for the
/// model, the denominator counts only sessions where it was primary. Python
/// calls that "the same convention the aggregator path uses"; both are the same
/// mismatch, so the port keeps it.
fn compare_models_from_marts(
    conn: &Connection,
    scope: &Scope,
    provider_filter: Option<&str>,
) -> Result<Vec<ModelStats>> {
    let mut model_totals = model_day_totals(conn, scope.since.as_deref(), scope.until.as_deref())?;

    let mut sessions_by_model: HashMap<String, i64> = HashMap::new();
    let mut one_shot_by_model: HashMap<String, i64> = HashMap::new();
    let mut assistant_msgs_by_model: HashMap<String, i64> = HashMap::new();
    let mut provider_by_model: HashMap<String, String> = HashMap::new();

    session_mart_rows_for_compare(
        conn,
        scope.since.as_deref(),
        scope.until.as_deref(),
        provider_filter,
        |row| {
            // `mdl = s.get("primary_model") or ""` — NULL and '' both drop out.
            if row.primary_model.is_empty() {
                return;
            }
            *sessions_by_model
                .entry(row.primary_model.clone())
                .or_insert(0) += 1;
            // `int(s.get("is_one_shot", 0) or 0) == 1` — exactly 1, so a 2 in
            // that column would NOT count.
            if row.is_one_shot == 1 {
                *one_shot_by_model
                    .entry(row.primary_model.clone())
                    .or_insert(0) += 1;
            }
            *assistant_msgs_by_model
                .entry(row.primary_model.clone())
                .or_insert(0) += row.assistant_message_count;
            // `provider_by_model.setdefault(...)` — FIRST row wins, and the row
            // order is whatever SQLite hands back (there is no ORDER BY).
            provider_by_model
                .entry(row.primary_model.clone())
                .or_insert_with(|| row.provider.clone());
            // Python also accumulates `cost_by_model` here and never reads it.
            // Not ported — DIV-087.
        },
    )?;

    // `if provider_filter is not None:` — an `is not None` test, NOT truthiness,
    // so `?provider=` (the empty string) takes this branch and restricts the
    // model list even though it filtered no session rows. DIV-086.
    if provider_filter.is_some() {
        model_totals.retain(|model, _| sessions_by_model.contains_key(model));
    }

    let mut out: Vec<ModelStats> = Vec::new();
    for (model, totals) in model_totals.into_pairs() {
        let calls = totals.message_count;
        if calls == 0 {
            // "Skip models with no events in window" — matches the aggregator
            // path, which never creates an accumulator for such a model.
            continue;
        }
        let cacheable = totals.cache_read + totals.cache_create;
        let sessions = sessions_by_model.get(&model).copied().unwrap_or(0);
        let one_shot = one_shot_by_model.get(&model).copied().unwrap_or(0);
        let assistant_msgs = assistant_msgs_by_model.get(&model).copied().unwrap_or(0);
        out.push(ModelStats {
            // `provider_by_model.get(mdl) or "anthropic"` — a model in
            // `model_day_mart` whose sessions all started outside the window has
            // no session row, so it inherits the legacy default.
            provider: provider_by_model
                .get(&model)
                .filter(|value| !value.is_empty())
                .cloned()
                .unwrap_or_else(|| "anthropic".to_owned()),
            model,
            sessions,
            calls,
            one_shot_pct: ratio(one_shot, sessions),
            retry_rate: retry_rate(assistant_msgs, sessions),
            cache_hit_rate: ratio(totals.cache_read, cacheable),
            // `if calls else 0.0` — dead, `calls == 0` returned above.
            cost_per_call: per_unit(totals.cost_usd, calls),
            cost_per_session: per_unit(totals.cost_usd, sessions),
            total_cost: totals.cost_usd,
            total_tokens: totals.input_tokens
                + totals.output_tokens
                + totals.cache_read
                + totals.cache_create,
        });
    }

    sort_by_total_cost_desc(&mut out);
    Ok(out)
}

// ── `store/mart_queries.py`, the four compare-facing reads ───────────────────

/// `_table_exists(conn, name)`.
fn table_exists(conn: &Connection, name: &str) -> Result<bool> {
    let mut stmt = conn.prepare("SELECT 1 FROM sqlite_master WHERE type='table' AND name=?")?;
    Ok(stmt.exists([name])?)
}

/// `mart_has_session_rows(conn)`.
///
/// # Errors
/// Any SQLite failure other than the table being absent, which is `false`.
fn mart_has_session_rows(conn: &Connection) -> Result<bool> {
    if !table_exists(conn, "session_mart")? {
        return Ok(false);
    }
    Ok(conn
        .prepare("SELECT 1 FROM session_mart LIMIT 1")?
        .exists([])?)
}

/// `mart_has_model_day_rows(conn)`.
fn mart_has_model_day_rows(conn: &Connection) -> Result<bool> {
    if !table_exists(conn, "model_day_mart")? {
        return Ok(false);
    }
    Ok(conn
        .prepare("SELECT 1 FROM model_day_mart LIMIT 1")?
        .exists([])?)
}

/// `_iso_to_day(iso_ts)` — the leading `YYYY-MM-DD` of an ISO stamp.
///
/// `len(iso_ts) < 10` and `iso_ts[:10]` count *characters*, not bytes; every
/// stamp in the store is ASCII, but the char-wise form is the one Python runs
/// and costs nothing to keep.
fn iso_to_day(iso: Option<&str>) -> Option<String> {
    // `if not iso_ts` — `None` and `""` both fall out here.
    let iso = iso.filter(|value| !value.is_empty())?;
    let chars: Vec<char> = iso.chars().collect();
    if chars.len() < 10 {
        return None;
    }
    Some(chars[..10].iter().collect())
}

/// `model_day_totals(conn, since_iso=…, until_iso=…)`.
///
/// The SQL text is `mart_queries.py`'s verbatim, including `WHERE 1=1` and the
/// `GROUP BY model` with no `ORDER BY` — the result order is SQLite's, and it is
/// the order the final tie-break inherits.
fn model_day_totals(
    conn: &Connection,
    since_iso: Option<&str>,
    until_iso: Option<&str>,
) -> Result<OrderedMap<ModelTotals>> {
    if !table_exists(conn, "model_day_mart")? {
        return Ok(OrderedMap::default());
    }
    let mut sql = String::from(
        "SELECT model, \
                SUM(cost_usd) AS cost_usd, \
                SUM(input_tokens) AS input_tokens, \
                SUM(output_tokens) AS output_tokens, \
                SUM(cache_read) AS cache_read, \
                SUM(cache_create) AS cache_create, \
                SUM(message_count) AS message_count \
         FROM model_day_mart WHERE 1=1",
    );
    let mut params: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
    if let Some(day) = iso_to_day(since_iso) {
        sql.push_str(" AND day >= ?");
        params.push(Box::new(day));
    }
    if let Some(day) = iso_to_day(until_iso) {
        sql.push_str(" AND day <= ?");
        params.push(Box::new(day));
    }
    sql.push_str(" GROUP BY model");

    let mut out: OrderedMap<ModelTotals> = OrderedMap::default();
    let mut stmt = conn.prepare(&sql)?;
    let refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(std::convert::AsRef::as_ref).collect();
    let mut rows = stmt.query(refs.as_slice())?;
    while let Some(row) = rows.next()? {
        // `model = row["model"] or ""` then `if not model: continue`.
        let model: String = row.get::<_, Option<String>>(0)?.unwrap_or_default();
        if model.is_empty() {
            continue;
        }
        // `float(row[…] or 0.0)` / `int(row[…] or 0)` — a SUM over zero rows is
        // NULL, and 0 is falsy, so both spellings collapse to the same zero.
        let totals = out.entry(&model);
        totals.cost_usd = row.get::<_, Option<f64>>(1)?.unwrap_or(0.0);
        totals.input_tokens = row.get::<_, Option<i64>>(2)?.unwrap_or(0);
        totals.output_tokens = row.get::<_, Option<i64>>(3)?.unwrap_or(0);
        totals.cache_read = row.get::<_, Option<i64>>(4)?.unwrap_or(0);
        totals.cache_create = row.get::<_, Option<i64>>(5)?.unwrap_or(0);
        totals.message_count = row.get::<_, Option<i64>>(6)?.unwrap_or(0);
    }
    Ok(out)
}

/// The five `session_mart` fields `services.compare` reads off each row.
struct SessionMartRow {
    provider: String,
    primary_model: String,
    assistant_message_count: i64,
    is_one_shot: i64,
}

/// `session_mart_rows_for_compare(conn, …)`.
///
/// The `SELECT` list is Python's full sixteen columns even though four are read:
/// narrowing it could let SQLite pick a covering index and hand back the rows in
/// a *different order*, and the row order decides which `provider` wins
/// `setdefault` for a model. Same text, same plan, same order.
fn session_mart_rows_for_compare(
    conn: &Connection,
    since_iso: Option<&str>,
    until_iso: Option<&str>,
    provider_filter: Option<&str>,
    mut visit: impl FnMut(&SessionMartRow),
) -> Result<()> {
    if !table_exists(conn, "session_mart")? {
        return Ok(());
    }
    let mut sql = String::from(
        "SELECT session_id, project_id, provider, primary_model, \
                first_ts, last_ts, \
                message_count, user_message_count, assistant_message_count, \
                input_tokens, output_tokens, cache_read, cache_create, \
                cost_usd, is_one_shot, cwd \
         FROM session_mart WHERE 1=1",
    );
    let mut params: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
    // Both bounds and the provider are truthiness-gated here (`if since_iso:`),
    // unlike the `is not None` test in `_compare_models_from_marts`.
    if let Some(since) = since_iso.filter(|value| !value.is_empty()) {
        sql.push_str(" AND first_ts >= ?");
        params.push(Box::new(since.to_owned()));
    }
    if let Some(until) = until_iso.filter(|value| !value.is_empty()) {
        sql.push_str(" AND first_ts <= ?");
        params.push(Box::new(until.to_owned()));
    }
    if let Some(provider) = provider_filter.filter(|value| !value.is_empty()) {
        // `LOWER(provider) = ?` against a lowered parameter — the only
        // case-insensitive filter in this module.
        sql.push_str(" AND LOWER(provider) = ?");
        params.push(Box::new(provider.to_lowercase()));
    }

    let mut stmt = conn.prepare(&sql)?;
    let refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(std::convert::AsRef::as_ref).collect();
    let mut rows = stmt.query(refs.as_slice())?;
    while let Some(row) = rows.next()? {
        visit(&SessionMartRow {
            provider: row.get::<_, Option<String>>(2)?.unwrap_or_default(),
            primary_model: row.get::<_, Option<String>>(3)?.unwrap_or_default(),
            assistant_message_count: row.get::<_, Option<i64>>(8)?.unwrap_or(0),
            is_one_shot: row.get::<_, Option<i64>>(14)?.unwrap_or(0),
        });
    }
    Ok(())
}

// ── the shared arithmetic ────────────────────────────────────────────────────

/// `(numerator / denominator) if denominator else 0.0`.
///
/// The guard's value is `0.0` and its TYPE is float — `0` would render as `0`
/// and change the bytes. LAW 3.
#[allow(
    clippy::cast_precision_loss,
    reason = "Python's int/int true division converts both operands to float first"
)]
fn ratio(numerator: i64, denominator: i64) -> f64 {
    if denominator == 0 {
        return 0.0;
    }
    numerator as f64 / denominator as f64
}

/// `(assistant_msgs / sessions - 1.0) if sessions else 0.0`.
///
/// Kept separate from [`ratio`] because the `- 1.0` happens *after* the
/// division and only on the non-zero leg: a model with no sessions gets `0.0`,
/// not `-1.0`.
#[allow(
    clippy::cast_precision_loss,
    reason = "Python's int/int true division converts both operands to float first"
)]
fn retry_rate(assistant_msgs: i64, sessions: i64) -> f64 {
    if sessions == 0 {
        return 0.0;
    }
    assistant_msgs as f64 / sessions as f64 - 1.0
}

/// `(total / count) if count else 0.0` — the dollar-per-unit shape.
#[allow(
    clippy::cast_precision_loss,
    reason = "Python promotes the int divisor to float before dividing"
)]
fn per_unit(total: f64, count: i64) -> f64 {
    if count == 0 {
        return 0.0;
    }
    total / count as f64
}

/// `out.sort(key=lambda r: r.total_cost, reverse=True)`.
///
/// CPython implements `reverse=True` as reverse-sort-reverse, which leaves the
/// relative order of EQUAL keys unchanged. `slice::sort_by` is stable, so a
/// descending comparator reproduces that exactly — `sort_unstable_by`, or a
/// sort followed by `reverse()`, would not.
fn sort_by_total_cost_desc(rows: &mut [ModelStats]) {
    rows.sort_by(|left, right| {
        right
            .total_cost
            .partial_cmp(&left.total_cost)
            // No arithmetic in this module can produce a NaN cost (every input
            // is a finite rate times an integer token count), so this arm is
            // unreachable; `Equal` keeps the pair in row order if it ever is.
            .unwrap_or(std::cmp::Ordering::Equal)
    });
}

/// A CPython `dict`: keyed lookup, insertion-ordered iteration.
///
/// `by_model` and `model_totals` are both plain dicts whose iteration order
/// decides the tie order of the final sort, so a `HashMap` alone would be a
/// nondeterministic port of a deterministic function.
#[derive(Debug)]
struct OrderedMap<T> {
    keys: Vec<String>,
    values: Vec<T>,
    index: HashMap<String, usize>,
}

impl<T> Default for OrderedMap<T> {
    fn default() -> Self {
        Self {
            keys: Vec::new(),
            values: Vec::new(),
            index: HashMap::new(),
        }
    }
}

impl<T: Default> OrderedMap<T> {
    /// `d.setdefault(key, T())` — appends on a miss, keeping insertion order.
    fn entry(&mut self, key: &str) -> &mut T {
        let slot = match self.index.get(key) {
            Some(slot) => *slot,
            None => {
                let slot = self.keys.len();
                self.keys.push(key.to_owned());
                self.values.push(T::default());
                self.index.insert(key.to_owned(), slot);
                slot
            }
        };
        &mut self.values[slot]
    }
}

impl<T> OrderedMap<T> {
    /// `{k: v for k, v in d.items() if predicate}` — order-preserving.
    fn retain(&mut self, mut predicate: impl FnMut(&str, &T) -> bool) {
        let mut keys = Vec::new();
        let mut values = Vec::new();
        let mut index = HashMap::new();
        for (key, value) in std::mem::take(&mut self.keys)
            .into_iter()
            .zip(std::mem::take(&mut self.values))
        {
            if predicate(&key, &value) {
                index.insert(key.clone(), keys.len());
                keys.push(key);
                values.push(value);
            }
        }
        self.keys = keys;
        self.values = values;
        self.index = index;
    }

    /// `d.items()`.
    fn into_pairs(self) -> impl Iterator<Item = (String, T)> {
        self.keys.into_iter().zip(self.values)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap as StdHashMap;
    use std::path::PathBuf;

    use stax_memory::pyjson::dumps_http;

    use super::*;

    /// 2026-07-15T12:00:00.000500+00:00 — mid-month, so `month` spans July.
    fn pinned() -> Instant {
        Instant::from_parts(2026, 7, 15, 12, 0, 0, 500)
    }

    /// The real rate card, with an overlay pinned on top so an assertion about
    /// arithmetic is not an assertion about today's manifest. `$1M/M tokens`
    /// makes one token cost one dollar, which keeps the expected bytes readable.
    fn engine() -> PricingEngine {
        let package = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../stackunderflow");
        let mut overlay = StdHashMap::new();
        overlay.insert("m-one".to_owned(), (1_000_000.0, 1_000_000.0, 0.0, 0.0));
        overlay.insert("m-two".to_owned(), (1_000_000.0, 1_000_000.0, 0.0, 0.0));
        PricingEngine::from_manifest_path(&crate::pricing::manifest_path(&package))
            .expect("the checked-in models.toml")
            .with_overlay(overlay)
    }

    fn store() -> Connection {
        let conn = Connection::open_in_memory().expect("in-memory");
        conn.execute_batch(
            "CREATE TABLE projects (id INTEGER PRIMARY KEY, slug TEXT, provider TEXT);
             CREATE TABLE sessions (id INTEGER PRIMARY KEY, project_id INTEGER);
             CREATE TABLE messages (
                 id INTEGER PRIMARY KEY, session_fk INTEGER, timestamp TEXT, role TEXT,
                 model TEXT, input_tokens INTEGER, output_tokens INTEGER,
                 cache_create_tokens INTEGER, cache_read_tokens INTEGER, speed TEXT);",
        )
        .expect("schema");
        conn
    }

    fn marts(conn: &Connection) {
        conn.execute_batch(
            "CREATE TABLE model_day_mart (
                 day TEXT NOT NULL, model TEXT NOT NULL, speed TEXT NOT NULL DEFAULT 'standard',
                 cost_usd REAL NOT NULL DEFAULT 0.0,
                 input_tokens INTEGER NOT NULL DEFAULT 0, output_tokens INTEGER NOT NULL DEFAULT 0,
                 cache_read INTEGER NOT NULL DEFAULT 0, cache_create INTEGER NOT NULL DEFAULT 0,
                 message_count INTEGER NOT NULL DEFAULT 0, session_count INTEGER NOT NULL DEFAULT 0,
                 PRIMARY KEY (day, model, speed));
             CREATE TABLE session_mart (
                 session_id TEXT PRIMARY KEY, project_id INTEGER NOT NULL,
                 provider TEXT NOT NULL, primary_model TEXT,
                 first_ts TEXT NOT NULL, last_ts TEXT NOT NULL,
                 message_count INTEGER NOT NULL DEFAULT 0,
                 user_message_count INTEGER NOT NULL DEFAULT 0,
                 assistant_message_count INTEGER NOT NULL DEFAULT 0,
                 input_tokens INTEGER NOT NULL DEFAULT 0, output_tokens INTEGER NOT NULL DEFAULT 0,
                 cache_read INTEGER NOT NULL DEFAULT 0, cache_create INTEGER NOT NULL DEFAULT 0,
                 cost_usd REAL NOT NULL DEFAULT 0.0, is_one_shot INTEGER NOT NULL DEFAULT 0,
                 cwd TEXT);",
        )
        .expect("mart schema");
    }

    /// One project, one provider, and `n` sessions.
    fn seed_projects(conn: &Connection) {
        conn.execute_batch(
            "INSERT INTO projects (id, slug, provider) VALUES (1, 'alpha', 'claude'),
                                                             (2, 'beta',  'codex');
             INSERT INTO sessions (id, project_id) VALUES (10, 1), (11, 1), (12, 2);",
        )
        .expect("seed");
    }

    fn message(
        conn: &Connection,
        session_fk: i64,
        role: &str,
        model: &str,
        input: i64,
        output: i64,
    ) {
        conn.execute(
            "INSERT INTO messages (session_fk, timestamp, role, model, input_tokens,
                                   output_tokens, cache_create_tokens, cache_read_tokens, speed)
             VALUES (?, '2026-07-10T09:00:00+00:00', ?, ?, ?, ?, 0, 0, 'standard')",
            rusqlite::params![session_fk, role, model, input, output],
        )
        .expect("insert");
    }

    #[test]
    fn week_is_the_only_alias_that_is_not_its_own_spec() {
        assert_eq!(period_spec("today"), Some("today"));
        assert_eq!(period_spec("week"), Some("7days"));
        assert_eq!(period_spec("month"), Some("month"));
        assert_eq!(period_spec("all"), Some("all"));
        assert_eq!(period_spec("7days"), None);
        assert_eq!(period_spec(""), None);
    }

    #[test]
    fn the_services_error_message_sorts_the_aliases_and_the_routes_does_not() {
        // `', '.join(sorted(PERIOD_MAP))` — alphabetical, not declaration order.
        assert_eq!(
            unknown_period_message("nope"),
            "Unknown period 'nope'. Valid: all, month, today, week"
        );
        // …while `routes/compare.py` joins the tuple. Both strings ship.
        assert_eq!(
            crate::routes::compare::unknown_period_detail("nope"),
            "Unknown period 'nope'. Valid: today, week, month, all"
        );
    }

    #[test]
    fn an_empty_store_renders_an_empty_models_list_not_a_null() {
        let conn = store();
        let payload =
            build_compare_payload(&conn, &engine(), "month", None, None, pinned(), || {
                1_700_000_000.5
            })
            .expect("empty store answers");
        assert_eq!(
            dumps_http(&payload),
            r#"{"period":"month","models":[],"generated":1700000000.5}"#
        );
    }

    #[test]
    fn the_echoed_period_is_the_alias_not_the_resolved_spec() {
        let conn = store();
        let payload = build_compare_payload(&conn, &engine(), "week", None, None, pinned(), || 0.0)
            .expect("week resolves");
        // `"7days"` would be the leak; the input string is what goes out.
        assert!(dumps_http(&payload).starts_with(r#"{"period":"week","models":[]"#));
    }

    #[test]
    fn a_zero_denominator_renders_a_float_zero_and_never_an_int_zero() {
        let conn = store();
        seed_projects(&conn);
        // One assistant message and no user message: the session is not
        // one-shot, so one_shot_pct is 0/1 — and no cacheable tokens at all, so
        // cache_hit_rate takes the GUARD, whose value must render `0.0`.
        message(&conn, 10, "assistant", "m-one", 3, 2);
        let rows = compare_models(
            &conn,
            &engine(),
            "month",
            Some(&["alpha".to_owned()]),
            None,
            pinned(),
        )
        .expect("rows");
        assert_eq!(rows.len(), 1);
        assert_eq!(
            dumps_http(&rows[0].to_value()),
            r#"{"model":"m-one","provider":"claude","sessions":1,"calls":1,"one_shot_pct":0.0,"retry_rate":0.0,"cache_hit_rate":0.0,"cost_per_call":5.0,"cost_per_session":5.0,"total_cost":5.0,"total_tokens":5}"#
        );
    }

    #[test]
    fn one_shot_needs_exactly_one_user_and_exactly_one_assistant_message() {
        let conn = store();
        seed_projects(&conn);
        // Session 10: 1 user + 1 assistant → one-shot.
        message(&conn, 10, "user", "", 0, 0);
        message(&conn, 10, "assistant", "m-one", 1, 0);
        // Session 11: 1 user + 2 assistant → not one-shot, and one retry.
        message(&conn, 11, "user", "", 0, 0);
        message(&conn, 11, "assistant", "m-one", 1, 0);
        message(&conn, 11, "assistant", "m-one", 1, 0);
        let rows = compare_models(
            &conn,
            &engine(),
            "month",
            Some(&["alpha".to_owned()]),
            None,
            pinned(),
        )
        .expect("rows");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].sessions, 2);
        assert_eq!(rows[0].calls, 3);
        // 1 one-shot / 2 sessions.
        assert!((rows[0].one_shot_pct - 0.5).abs() < f64::EPSILON);
        // 3 assistant messages / 2 sessions - 1.
        assert!((rows[0].retry_rate - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn a_model_free_assistant_row_is_skipped_for_cost_but_still_counts_as_a_retry() {
        let conn = store();
        seed_projects(&conn);
        message(&conn, 10, "user", "", 0, 0);
        message(&conn, 10, "assistant", "m-one", 4, 0);
        // No model recorded: excluded from `by_model`, so it adds no call and no
        // cost — but `per_session_assistant` counted it, so the retry rate of
        // `m-one` (which wins the session) moves. That asymmetry is inherited.
        message(&conn, 10, "assistant", "", 999, 999);
        let rows = compare_models(
            &conn,
            &engine(),
            "month",
            Some(&["alpha".to_owned()]),
            None,
            pinned(),
        )
        .expect("rows");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].calls, 1);
        assert_eq!(rows[0].total_tokens, 4);
        // 2 assistant messages / 1 session - 1 == 1.0, from one priced call.
        assert!((rows[0].retry_rate - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn the_primary_model_of_a_session_is_the_most_used_id_with_ties_going_to_the_lowest() {
        let mut counts = HashMap::new();
        counts.insert("zeta".to_owned(), 3);
        counts.insert("alpha".to_owned(), 3);
        assert_eq!(primary_model_for_session(&counts), "alpha");

        // A real id beats the empty string even at the SAME count, because the
        // loop skips falsy candidates rather than taking `candidates[0]`.
        let mut counts = HashMap::new();
        counts.insert(String::new(), 5);
        counts.insert("zeta".to_owned(), 5);
        assert_eq!(primary_model_for_session(&counts), "zeta");

        // …but a higher count still wins outright, even for the empty string.
        let mut counts = HashMap::new();
        counts.insert(String::new(), 9);
        counts.insert("zeta".to_owned(), 1);
        assert_eq!(primary_model_for_session(&counts), "");

        assert_eq!(primary_model_for_session(&HashMap::new()), "");
    }

    #[test]
    fn rows_sort_by_cost_descending_and_equal_costs_keep_their_row_order() {
        let mut rows = vec![
            ModelStats {
                model: "first-seen".to_owned(),
                provider: "claude".to_owned(),
                sessions: 1,
                calls: 1,
                one_shot_pct: 0.0,
                retry_rate: 0.0,
                cache_hit_rate: 0.0,
                cost_per_call: 0.0,
                cost_per_session: 0.0,
                total_cost: 2.0,
                total_tokens: 0,
            },
            ModelStats {
                model: "second-seen".to_owned(),
                total_cost: 2.0,
                ..rows_template()
            },
            ModelStats {
                model: "expensive".to_owned(),
                total_cost: 9.0,
                ..rows_template()
            },
        ];
        sort_by_total_cost_desc(&mut rows);
        let order: Vec<&str> = rows.iter().map(|row| row.model.as_str()).collect();
        // A `sort_unstable_by`, or an ascending sort plus `reverse()`, would
        // swap the two 2.0 rows. CPython's `reverse=True` does not.
        assert_eq!(order, vec!["expensive", "first-seen", "second-seen"]);
    }

    fn rows_template() -> ModelStats {
        ModelStats {
            model: String::new(),
            provider: "claude".to_owned(),
            sessions: 1,
            calls: 1,
            one_shot_pct: 0.0,
            retry_rate: 0.0,
            cache_hit_rate: 0.0,
            cost_per_call: 0.0,
            cost_per_session: 0.0,
            total_cost: 0.0,
            total_tokens: 0,
        }
    }

    #[test]
    fn a_populated_mart_wins_over_the_messages_table_and_reprices_nothing() {
        let conn = store();
        seed_projects(&conn);
        marts(&conn);
        // The messages table says one cheap call; the mart says something else
        // entirely. With no project filter the MART is the answer, and the
        // pricing engine is never consulted.
        message(&conn, 10, "assistant", "m-one", 1, 1);
        conn.execute_batch(
            "INSERT INTO model_day_mart (day, model, cost_usd, input_tokens, output_tokens,
                                         cache_read, cache_create, message_count, session_count)
             VALUES ('2026-07-10', 'm-one', 12.5, 100, 20, 30, 10, 4, 1);
             INSERT INTO session_mart (session_id, project_id, provider, primary_model,
                                       first_ts, last_ts, assistant_message_count,
                                       cost_usd, is_one_shot)
             VALUES ('s1', 1, 'claude', 'm-one', '2026-07-10T09:00:00+00:00',
                     '2026-07-10T10:00:00+00:00', 4, 12.5, 1);",
        )
        .expect("mart rows");
        let rows =
            compare_models(&conn, &engine(), "month", None, None, pinned()).expect("mart path");
        assert_eq!(rows.len(), 1);
        assert_eq!(
            dumps_http(&rows[0].to_value()),
            // cache_hit_rate = 30/(30+10); cost_per_call = 12.5/4;
            // cost_per_session = 12.5/1; retry_rate = 4/1 - 1.
            r#"{"model":"m-one","provider":"claude","sessions":1,"calls":4,"one_shot_pct":1.0,"retry_rate":3.0,"cache_hit_rate":0.75,"cost_per_call":3.125,"cost_per_session":12.5,"total_cost":12.5,"total_tokens":160}"#
        );
    }

    #[test]
    fn a_project_filter_forces_the_messages_path_even_with_both_marts_populated() {
        let conn = store();
        seed_projects(&conn);
        marts(&conn);
        message(&conn, 10, "assistant", "m-one", 1, 1);
        conn.execute_batch(
            "INSERT INTO model_day_mart (day, model, cost_usd, message_count)
             VALUES ('2026-07-10', 'mart-only', 99.0, 7);
             INSERT INTO session_mart (session_id, project_id, provider, primary_model,
                                       first_ts, last_ts, assistant_message_count, is_one_shot)
             VALUES ('s1', 1, 'claude', 'mart-only', '2026-07-10T09:00:00+00:00',
                     '2026-07-10T10:00:00+00:00', 7, 0);",
        )
        .expect("mart rows");
        let rows = compare_models(
            &conn,
            &engine(),
            "month",
            Some(&["alpha".to_owned()]),
            None,
            pinned(),
        )
        .expect("fallback path");
        // `model_day_mart` has no `project_id`, so a slug filter cannot be
        // satisfied there and the raw join answers instead.
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].model, "m-one");
        assert!((rows[0].total_cost - 2.0).abs() < f64::EPSILON);
    }

    #[test]
    fn an_empty_provider_string_filters_no_sessions_but_still_prunes_the_model_list() {
        let conn = store();
        seed_projects(&conn);
        marts(&conn);
        conn.execute_batch(
            "INSERT INTO model_day_mart (day, model, cost_usd, message_count)
             VALUES ('2026-07-10', 'has-sessions', 5.0, 2),
                    ('2026-07-10', 'no-sessions',  9.0, 2);
             INSERT INTO session_mart (session_id, project_id, provider, primary_model,
                                       first_ts, last_ts, assistant_message_count, is_one_shot)
             VALUES ('s1', 1, 'claude', 'has-sessions', '2026-07-10T09:00:00+00:00',
                     '2026-07-10T10:00:00+00:00', 2, 0);",
        )
        .expect("mart rows");

        // No filter at all: both models survive, `no-sessions` inheriting the
        // legacy "anthropic" provider default.
        let rows =
            compare_models(&conn, &engine(), "month", None, None, pinned()).expect("no filter");
        let models: Vec<&str> = rows.iter().map(|row| row.model.as_str()).collect();
        assert_eq!(models, vec!["no-sessions", "has-sessions"]);
        assert_eq!(rows[0].provider, "anthropic");

        // `?provider=` — an EMPTY string. `if provider_filter:` in the SQL layer
        // is false so no session row is filtered, but `if provider_filter is not
        // None:` in the caller is TRUE, so the model list is pruned to models
        // that have a session. Both tests are on the same variable. DIV-086.
        let rows = compare_models(&conn, &engine(), "month", None, Some(""), pinned())
            .expect("empty provider");
        let models: Vec<&str> = rows.iter().map(|row| row.model.as_str()).collect();
        assert_eq!(models, vec!["has-sessions"]);
    }

    #[test]
    fn the_provider_filter_is_case_insensitive_on_the_mart_path_and_exact_on_the_other() {
        let conn = store();
        seed_projects(&conn);
        marts(&conn);
        conn.execute_batch(
            "INSERT INTO model_day_mart (day, model, cost_usd, message_count)
             VALUES ('2026-07-10', 'm-one', 5.0, 2);
             INSERT INTO session_mart (session_id, project_id, provider, primary_model,
                                       first_ts, last_ts, assistant_message_count, is_one_shot)
             VALUES ('s1', 1, 'claude', 'm-one', '2026-07-10T09:00:00+00:00',
                     '2026-07-10T10:00:00+00:00', 2, 0);",
        )
        .expect("mart rows");
        // `LOWER(provider) = ?` with a lowered parameter — CLAUDE matches.
        let rows = compare_models(&conn, &engine(), "month", None, Some("CLAUDE"), pinned())
            .expect("mart path");
        assert_eq!(rows.len(), 1);

        // The messages path compares `projects.provider = ?` raw, so the same
        // spelling matches nothing there.
        message(&conn, 10, "assistant", "m-one", 1, 1);
        let rows = compare_models(
            &conn,
            &engine(),
            "month",
            Some(&["alpha".to_owned()]),
            Some("CLAUDE"),
            pinned(),
        )
        .expect("messages path");
        assert!(rows.is_empty());
    }

    #[test]
    fn a_mart_model_with_no_events_in_the_window_is_dropped_before_it_can_divide_by_zero() {
        let conn = store();
        seed_projects(&conn);
        marts(&conn);
        conn.execute_batch(
            "INSERT INTO model_day_mart (day, model, cost_usd, message_count)
             VALUES ('2026-07-10', 'ghost', 0.0, 0), ('2026-07-10', 'real', 1.0, 1);",
        )
        .expect("mart rows");
        // `session_mart` must have a row or the fast path is not taken at all.
        conn.execute_batch(
            "INSERT INTO session_mart (session_id, project_id, provider, primary_model,
                                       first_ts, last_ts, assistant_message_count, is_one_shot)
             VALUES ('s1', 1, 'claude', 'real', '2026-07-10T09:00:00+00:00',
                     '2026-07-10T10:00:00+00:00', 1, 1);",
        )
        .expect("session row");
        let rows = compare_models(&conn, &engine(), "month", None, None, pinned()).expect("rows");
        let models: Vec<&str> = rows.iter().map(|row| row.model.as_str()).collect();
        assert_eq!(models, vec!["real"]);
    }

    #[test]
    fn one_empty_mart_is_enough_to_send_the_whole_request_down_the_fallback() {
        let conn = store();
        seed_projects(&conn);
        marts(&conn);
        message(&conn, 10, "assistant", "m-one", 1, 1);
        // `model_day_mart` populated, `session_mart` empty: the AND in
        // `compare_models` means the marts are not used at all.
        conn.execute_batch(
            "INSERT INTO model_day_mart (day, model, cost_usd, message_count)
             VALUES ('2026-07-10', 'mart-only', 99.0, 7);",
        )
        .expect("mart row");
        let rows = compare_models(&conn, &engine(), "month", None, None, pinned()).expect("rows");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].model, "m-one");
    }

    #[test]
    fn the_day_slice_needs_ten_characters_and_a_shorter_stamp_drops_the_bound() {
        assert_eq!(
            iso_to_day(Some("2026-07-15T12:00:00+00:00")).as_deref(),
            Some("2026-07-15")
        );
        assert_eq!(
            iso_to_day(Some("2026-07-15")).as_deref(),
            Some("2026-07-15")
        );
        // Nine characters: `len(iso_ts) < 10` returns None, so the mart query
        // runs UNBOUNDED on that side rather than with a truncated bound.
        assert_eq!(iso_to_day(Some("2026-07-1")), None);
        assert_eq!(iso_to_day(Some("")), None);
        assert_eq!(iso_to_day(None), None);
    }

    #[test]
    fn the_today_window_prunes_the_mart_by_day_string_not_by_timestamp() {
        let conn = store();
        seed_projects(&conn);
        marts(&conn);
        conn.execute_batch(
            "INSERT INTO model_day_mart (day, model, cost_usd, message_count)
             VALUES ('2026-07-14', 'yesterday', 1.0, 1), ('2026-07-15', 'today', 2.0, 1);
             INSERT INTO session_mart (session_id, project_id, provider, primary_model,
                                       first_ts, last_ts, assistant_message_count, is_one_shot)
             VALUES ('s1', 1, 'claude', 'today', '2026-07-15T09:00:00+00:00',
                     '2026-07-15T10:00:00+00:00', 1, 1);",
        )
        .expect("mart rows");
        let rows = compare_models(&conn, &engine(), "today", None, None, pinned()).expect("rows");
        let models: Vec<&str> = rows.iter().map(|row| row.model.as_str()).collect();
        assert_eq!(models, vec!["today"]);
    }

    #[test]
    fn the_epoch_float_is_built_the_way_cpython_builds_it() {
        // A whole number of seconds takes the integer branch exactly.
        assert!(
            (py_time_as_seconds_double(1_700_000_000_000_000_000) - 1.7e9).abs() < f64::EPSILON
        );
        // A sub-second remainder takes the (double)ns / 1e9 branch — NOT
        // `secs as f64 + nanos as f64 / 1e9`, which rounds twice.
        let ours = py_time_as_seconds_double(1_700_000_000_123_456_789);
        #[allow(clippy::cast_precision_loss, reason = "the point of the comparison")]
        let naive = 1_700_000_000_f64 + 123_456_789_f64 / 1e9;
        assert!((ours - naive).abs() < 1e-6, "same instant, different bits");
        // And it is a live clock, not a constant.
        assert!(now_unix_seconds() > 1.7e9);
    }
}
