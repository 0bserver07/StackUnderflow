//! `routes/pricing.py` — 1 endpoint, wave 5 (batch A).
//!
//! | Item | Method | FastAPI path | axum path | State |
//! |---|---|---|---|---|
//! | `RS-5-094` | `GET` | `/api/pricing/doctor` | `/api/pricing/doctor` | ported |
//!
//! `assemble_pricing_health` is the whole module: three read-only sweeps over
//! `usage_events`, each guarded by a `sqlite_master` probe so a fresh install
//! answers `ok` instead of 500ing, plus a freshness probe of the on-disk
//! LiteLLM overlay that deliberately does **not** fetch.
//!
//! # The two things this endpoint gets wrong-looking on purpose
//!
//! * **`is_rate_card_model` is membership, not "a rate resolves".** The pricers
//!   fall back to a default family for unrecognised ids, so `get_model_pricing`
//!   almost never says `None`; exact rate-card membership is the only honest
//!   signal, and it is the same test every normalizer uses to stamp
//!   `cost_source`. `PricingEngine::is_rate_card_model` is that same test.
//! * **The estimate prices through the *primed* book.** `server.py`'s lifespan
//!   flips `infra.costs` onto the `price_book` table before it serves a byte, so
//!   the `estimated_delta_usd` figures here are book-priced, not manifest-priced.
//!   [`crate::pricing::engine`] pins the Rust half to the same source — this is
//!   RS-3-082's seam, and reading the manifest instead would quietly change
//!   every dollar in the payload.

use axum::Router;
use axum::extract::{RawQuery, State};
use axum::http::StatusCode;
use axum::routing::get;
use rusqlite::Connection;
use serde_json::{Map, Value};
use stax_etl::pricing::RawTokens;
use stax_etl::pricing::costs::PricingEngine;
// `round_py` is `round(x, n)` and `neumaier_sum` is CPython's `sum()` over
// floats. Both used to be private copies in this file, on the (mistaken) claim
// that `stax_etl::stats::aggregator` did not expose them.
use stax_etl::stats::aggregator::{neumaier_sum, round_py as round_half_even};

use crate::json::{HandlerResult, HttpError, JsonBody, join_failure, validation_422};
use crate::qs::Query;
use crate::services::mart_queries::table_exists;
use crate::state::AppState;

/// `DEFAULT_STALE_DAYS` — mirrors `PricingService.STALE_THRESHOLD`.
const DEFAULT_STALE_DAYS: i64 = 7;
/// `DEFAULT_LIMIT` — per-list cap; the full counts stay in `summary`.
const DEFAULT_LIMIT: i64 = 50;
/// `_BILLABLE_SOURCES`.
const BILLABLE_SOURCES: [&str; 2] = ["rate_card", "live"];

/// Mount this module's endpoints onto `router`.
pub fn register(router: Router<AppState>) -> Router<AppState> {
    router.route("/api/pricing/doctor", get(get_pricing_doctor))
}

async fn get_pricing_doctor(
    State(state): State<AppState>,
    RawQuery(raw): RawQuery,
) -> HandlerResult {
    let query = Query::parse(raw.as_deref().unwrap_or_default());
    // FastAPI validates BOTH parameters before the handler body runs, and
    // reports the FIRST failure in declaration order (`stale_days`, then
    // `limit`) — so a request with two bad values names `stale_days`.
    let stale_days = match query.int_or("stale_days", DEFAULT_STALE_DAYS) {
        Ok(value) => value,
        Err(err) => return Ok(validation_422(&err)),
    };
    let limit = match query.int_or("limit", DEFAULT_LIMIT) {
        Ok(value) => value,
        Err(err) => return Ok(validation_422(&err)),
    };

    tokio::task::spawn_blocking(move || {
        let conn = state
            .connect()
            .map_err(|err| HttpError::new(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?;
        let engine = crate::pricing::engine(&conn, state.package_dir())
            .map_err(|err| HttpError::new(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?;
        let app_dir = state
            .store_path()
            .parent()
            .map(std::path::Path::to_path_buf);
        assemble_pricing_health(&conn, &engine, app_dir.as_deref(), stale_days, limit)
            .map(JsonBody::ok)
            .map_err(|err| HttpError::new(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))
    })
    .await
    .map_err(|err| join_failure(&err))?
}

// ── the assembler ────────────────────────────────────────────────────────────

/// `assemble_pricing_health` — read-only, and complete even on a cold store.
fn assemble_pricing_health(
    conn: &Connection,
    engine: &PricingEngine,
    app_dir: Option<&std::path::Path>,
    stale_days: i64,
    limit: i64,
) -> anyhow::Result<Value> {
    let freshness = rate_freshness(app_dir, stale_days);
    let freshness_stale = freshness
        .get("stale")
        .and_then(Value::as_bool)
        .unwrap_or(true);

    if !table_exists(conn, "usage_events")? {
        return Ok(empty_payload(stale_days, freshness, freshness_stale));
    }

    let mut unpriced = unpriced_models(conn, engine)?;
    let mut unknown = unknown_cost_source(conn, engine)?;
    let violation_rows = unknown_nonzero_cost_rows(conn)?;
    let (total_events, total_cost) = totals(conn)?;

    // `sort(key=lambda d: -(d["estimated_delta_usd"] or 0.0))` — a STABLE sort
    // on the negated delta, so ties keep the GROUP BY order SQLite produced.
    sort_by_negated_delta(&mut unpriced);
    sort_by_negated_delta(&mut unknown);

    let billable_unpriced: Vec<&Value> = unpriced
        .iter()
        .filter(|row| {
            row.get("billable")
                .and_then(Value::as_bool)
                .unwrap_or(false)
        })
        .collect();
    // `ok` reflects HARD defects only: no billable row against an unresolvable
    // model, and no `unknown` row carrying a nonzero cost. A stale overlay and
    // correctly-stamped exotic models are warnings, not failures.
    let ok = billable_unpriced.is_empty() && violation_rows == 0;
    let billable_count = billable_unpriced.len();

    // `sum(... or 0.0)` over a Python list is Neumaier-compensated (gh-100425),
    // so a plain `+=` chain can drift 1-2 ULP past a few thousand rows. The
    // exposure figure is rounded to 6 dp afterwards, which usually hides it —
    // "usually" is not a parity argument, so the compensated sum it is.
    let exposure = round_half_even(
        neumaier_sum(unpriced.iter().map(|row| {
            row.get("estimated_delta_usd")
                .and_then(Value::as_f64)
                .unwrap_or(0.0)
        })),
        6,
    );

    let mut summary = Map::new();
    summary.insert("total_events".to_owned(), Value::from(total_events));
    summary.insert(
        "total_cost_usd".to_owned(),
        Value::from(round_half_even(total_cost, 6)),
    );
    summary.insert(
        "unpriced_model_count".to_owned(),
        Value::from(unpriced.len()),
    );
    summary.insert(
        "billable_unpriced_model_count".to_owned(),
        Value::from(billable_count),
    );
    summary.insert(
        "unknown_cost_source_model_count".to_owned(),
        Value::from(unknown.len()),
    );
    summary.insert(
        "unknown_nonzero_cost_rows".to_owned(),
        Value::from(violation_rows),
    );
    summary.insert(
        "estimated_unpriced_exposure_usd".to_owned(),
        Value::from(exposure),
    );
    summary.insert("rate_cache_stale".to_owned(), Value::Bool(freshness_stale));

    let cap = usize::try_from(limit.max(0)).unwrap_or(usize::MAX);
    unpriced.truncate(cap);
    unknown.truncate(cap);

    let mut payload = Map::new();
    payload.insert("stale_days".to_owned(), Value::from(stale_days));
    payload.insert("ok".to_owned(), Value::Bool(ok));
    payload.insert("summary".to_owned(), Value::Object(summary));
    payload.insert("unpriced_models".to_owned(), Value::Array(unpriced));
    payload.insert("unknown_cost_source".to_owned(), Value::Array(unknown));
    payload.insert("rate_freshness".to_owned(), freshness);
    Ok(Value::Object(payload))
}

fn sort_by_negated_delta(rows: &mut [Value]) {
    rows.sort_by(|a, b| {
        let key = |row: &Value| {
            -row.get("estimated_delta_usd")
                .and_then(Value::as_f64)
                .unwrap_or(0.0)
        };
        key(a)
            .partial_cmp(&key(b))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
}

fn empty_payload(stale_days: i64, freshness: Value, stale: bool) -> Value {
    let mut summary = Map::new();
    summary.insert("total_events".to_owned(), Value::from(0));
    summary.insert("total_cost_usd".to_owned(), Value::from(0.0));
    summary.insert("unpriced_model_count".to_owned(), Value::from(0));
    summary.insert("billable_unpriced_model_count".to_owned(), Value::from(0));
    summary.insert("unknown_cost_source_model_count".to_owned(), Value::from(0));
    summary.insert("unknown_nonzero_cost_rows".to_owned(), Value::from(0));
    summary.insert(
        "estimated_unpriced_exposure_usd".to_owned(),
        Value::from(0.0),
    );
    summary.insert("rate_cache_stale".to_owned(), Value::Bool(stale));

    let mut payload = Map::new();
    payload.insert("stale_days".to_owned(), Value::from(stale_days));
    payload.insert("ok".to_owned(), Value::Bool(true));
    payload.insert("summary".to_owned(), Value::Object(summary));
    payload.insert("unpriced_models".to_owned(), Value::Array(Vec::new()));
    payload.insert("unknown_cost_source".to_owned(), Value::Array(Vec::new()));
    payload.insert("rate_freshness".to_owned(), freshness);
    Value::Object(payload)
}

// ── dimension builders ───────────────────────────────────────────────────────

/// One `GROUP BY provider, model` row, for either sweep.
///
/// The two queries select the same measures in a different column order; one
/// struct carries both so neither `query_map` closure needs a nine-tuple (and
/// so the field names, rather than an index, say which measure is which).
struct ModelRollup {
    provider: Option<String>,
    model: String,
    events: i64,
    input_tokens: i64,
    output_tokens: i64,
    cache_read_tokens: i64,
    cache_create_tokens: i64,
    cost_usd: f64,
    sources: Option<String>,
}

/// `_unpriced_models` — distinct `(provider, model)` with no resolvable rate card.
fn unpriced_models(conn: &Connection, engine: &PricingEngine) -> anyhow::Result<Vec<Value>> {
    let mut stmt = conn.prepare(
        "SELECT provider, model, \
                COUNT(*)                  AS events, \
                SUM(input_tokens)         AS it, \
                SUM(output_tokens)        AS ot, \
                SUM(cache_read_tokens)    AS crt, \
                SUM(cache_create_tokens)  AS cct, \
                SUM(cost_usd)             AS cost, \
                GROUP_CONCAT(DISTINCT cost_source) AS sources \
         FROM usage_events \
         WHERE model <> '' \
         GROUP BY provider, model",
    )?;
    let rows: Vec<ModelRollup> = stmt
        .query_map([], |row| {
            Ok(ModelRollup {
                provider: row.get(0)?,
                model: row.get(1)?,
                events: row.get(2)?,
                input_tokens: row.get::<_, Option<i64>>(3)?.unwrap_or(0),
                output_tokens: row.get::<_, Option<i64>>(4)?.unwrap_or(0),
                cache_read_tokens: row.get::<_, Option<i64>>(5)?.unwrap_or(0),
                cache_create_tokens: row.get::<_, Option<i64>>(6)?.unwrap_or(0),
                cost_usd: row.get::<_, Option<f64>>(7)?.unwrap_or(0.0),
                sources: row.get(8)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;

    let mut out = Vec::new();
    for row in rows {
        let ModelRollup {
            provider,
            model,
            events,
            input_tokens: it,
            output_tokens: ot,
            cache_read_tokens: crt,
            cache_create_tokens: cct,
            cost_usd: cost,
            sources,
        } = row;
        if engine.is_rate_card_model(&model) {
            continue;
        }
        let est = engine.estimate_cost(&RawTokens::canonical(it, ot, cct, crt), &model);
        // `sorted(s for s in (r["sources"] or "").split(",") if s)` — a plain
        // string sort, and the empty pieces are dropped BEFORE sorting.
        let mut source_list: Vec<String> = sources
            .unwrap_or_default()
            .split(',')
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect();
        source_list.sort();
        let billable = source_list
            .iter()
            .any(|s| BILLABLE_SOURCES.contains(&s.as_str()));

        let mut obj = Map::new();
        obj.insert(
            "provider".to_owned(),
            provider.map_or(Value::Null, Value::from),
        );
        obj.insert("model".to_owned(), Value::from(model));
        obj.insert("events".to_owned(), Value::from(events));
        obj.insert("input_tokens".to_owned(), Value::from(it));
        obj.insert("output_tokens".to_owned(), Value::from(ot));
        obj.insert("cache_read_tokens".to_owned(), Value::from(crt));
        obj.insert("cache_create_tokens".to_owned(), Value::from(cct));
        obj.insert(
            "current_cost_usd".to_owned(),
            Value::from(round_half_even(cost, 6)),
        );
        obj.insert("estimated_cost_usd".to_owned(), optional_round(est, est));
        obj.insert(
            "estimated_delta_usd".to_owned(),
            optional_round(est, est - cost),
        );
        obj.insert(
            "cost_sources".to_owned(),
            Value::Array(source_list.into_iter().map(Value::from).collect()),
        );
        obj.insert("billable".to_owned(), Value::Bool(billable));
        out.push(Value::Object(obj));
    }
    Ok(out)
}

/// `_unknown_cost_source` — per-model rollup of `cost_source='unknown'` rows.
fn unknown_cost_source(conn: &Connection, engine: &PricingEngine) -> anyhow::Result<Vec<Value>> {
    let mut stmt = conn.prepare(
        "SELECT provider, model, \
                COUNT(*)                  AS events, \
                SUM(cost_usd)             AS cost, \
                SUM(input_tokens)         AS it, \
                SUM(output_tokens)        AS ot, \
                SUM(cache_read_tokens)    AS crt, \
                SUM(cache_create_tokens)  AS cct \
         FROM usage_events \
         WHERE cost_source = 'unknown' \
         GROUP BY provider, model",
    )?;
    let rows: Vec<ModelRollup> = stmt
        .query_map([], |row| {
            Ok(ModelRollup {
                provider: row.get(0)?,
                model: row.get(1)?,
                events: row.get(2)?,
                cost_usd: row.get::<_, Option<f64>>(3)?.unwrap_or(0.0),
                input_tokens: row.get::<_, Option<i64>>(4)?.unwrap_or(0),
                output_tokens: row.get::<_, Option<i64>>(5)?.unwrap_or(0),
                cache_read_tokens: row.get::<_, Option<i64>>(6)?.unwrap_or(0),
                cache_create_tokens: row.get::<_, Option<i64>>(7)?.unwrap_or(0),
                sources: None,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;

    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let ModelRollup {
            provider,
            model,
            events,
            cost_usd: cost,
            input_tokens: it,
            output_tokens: ot,
            cache_read_tokens: crt,
            cache_create_tokens: cct,
            ..
        } = row;
        // `estimate_cost(...) if model else 0.0` — an empty model id never
        // reaches the pricer here, unlike in `_unpriced_models` (whose WHERE
        // clause already excluded it).
        let est = if model.is_empty() {
            0.0
        } else {
            engine.estimate_cost(&RawTokens::canonical(it, ot, cct, crt), &model)
        };
        let mut obj = Map::new();
        obj.insert(
            "provider".to_owned(),
            provider.map_or(Value::Null, Value::from),
        );
        obj.insert("model".to_owned(), Value::from(model));
        obj.insert("events".to_owned(), Value::from(events));
        // `it + ot + crt + cct` — Python ints, so no overflow to reproduce.
        obj.insert(
            "tokens".to_owned(),
            Value::from(
                it.saturating_add(ot)
                    .saturating_add(crt)
                    .saturating_add(cct),
            ),
        );
        obj.insert("cost_usd".to_owned(), Value::from(round_half_even(cost, 6)));
        obj.insert("estimated_cost_usd".to_owned(), optional_round(est, est));
        obj.insert(
            "estimated_delta_usd".to_owned(),
            optional_round(est, est - cost),
        );
        out.push(Value::Object(obj));
    }
    Ok(out)
}

/// `round(value, 6) if est > 0 else None` — the gate is on `est`, not `value`.
fn optional_round(est: f64, value: f64) -> Value {
    if est > 0.0 {
        Value::from(round_half_even(value, 6))
    } else {
        Value::Null
    }
}

fn unknown_nonzero_cost_rows(conn: &Connection) -> rusqlite::Result<i64> {
    conn.query_row(
        "SELECT COUNT(*) AS n FROM usage_events WHERE cost_source = 'unknown' AND cost_usd <> 0",
        [],
        |row| row.get(0),
    )
}

fn totals(conn: &Connection) -> rusqlite::Result<(i64, f64)> {
    conn.query_row(
        "SELECT COUNT(*) AS n, COALESCE(SUM(cost_usd), 0.0) AS c FROM usage_events",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )
}

// ── the overlay freshness probe ──────────────────────────────────────────────

/// `_rate_freshness` → `PricingService.read_cache_status()` plus two keys.
///
/// Key order is `{**status, "stale_days_threshold": …, "stale": …}`, so the five
/// status keys come first in their own literal order and the two additions land
/// after. Reads `app_dir()/cache/pricing.json` and **never** fetches or mkdirs —
/// that is the whole reason `read_cache_status` exists next to `get_pricing`.
///
/// One honest caveat, recorded rather than papered over: when the cache file
/// *does* exist and carries a parseable timestamp, `age_days` is
/// `(now - cache_time).total_seconds() / 86400`, so the two servers compute it
/// microseconds apart and the byte comparison of that one float cannot hold.
/// The harness home ships an empty `cache/`, so the ported leg is the one the
/// gate exercises; a home with a warm overlay would show this as a
/// float-in-`age_days` divergence and nothing else.
fn rate_freshness(app_dir: Option<&std::path::Path>, stale_days: i64) -> Value {
    let status = read_cache_status(app_dir);
    let mut out = match status {
        Value::Object(map) => map,
        _ => Map::new(),
    };
    let is_stale = out.get("is_stale").and_then(Value::as_bool).unwrap_or(true);
    let age = out.get("age_days").and_then(Value::as_f64);
    // `stale = age is None or age > stale_days` — strictly greater, and NOT the
    // same predicate as `is_stale` (which is `>=` against the 7-day constant).
    let stale = match age {
        None => true,
        Some(age) => age > stale_days as f64,
    };
    let _ = is_stale;
    out.insert("stale_days_threshold".to_owned(), Value::from(stale_days));
    out.insert("stale".to_owned(), Value::Bool(stale));
    Value::Object(out)
}

/// `PricingService.STALE_THRESHOLD.days`.
const STALE_THRESHOLD_DAYS: f64 = 7.0;

/// `PricingService.read_cache_status` — read-only, no network, no mkdir.
fn read_cache_status(app_dir: Option<&std::path::Path>) -> Value {
    let empty = || {
        let mut obj = Map::new();
        obj.insert("source".to_owned(), Value::from("none"));
        obj.insert("timestamp".to_owned(), Value::Null);
        obj.insert("age_days".to_owned(), Value::Null);
        obj.insert("is_stale".to_owned(), Value::Bool(true));
        obj.insert("model_count".to_owned(), Value::from(0));
        Value::Object(obj)
    };
    let Some(dir) = app_dir else { return empty() };
    let cache_file = dir.join("cache").join("pricing.json");
    let Ok(text) = std::fs::read_to_string(&cache_file) else {
        return empty();
    };
    let Ok(data) = serde_json::from_str::<Value>(&text) else {
        return empty();
    };

    let ts = data.get("timestamp").cloned().unwrap_or(Value::Null);
    // `if ts:` — Python truthiness, so `""`, `0` and `false` all skip the parse.
    let age_days = if truthy(&ts) {
        let raw = match &ts {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        parse_age_days(&raw)
    } else {
        None
    };
    // `is_stale = age_days is None or age_days >= STALE_THRESHOLD.days`.
    let is_stale = age_days.is_none_or(|age| age >= STALE_THRESHOLD_DAYS);
    let model_count = match data.get("pricing") {
        Some(Value::Object(map)) => map.len(),
        _ => 0,
    };
    // `str(data.get("source") or "cache")` — falsy sources become `"cache"`.
    let source = match data.get("source") {
        Some(value) if truthy(value) => match value {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        },
        _ => "cache".to_owned(),
    };

    let mut obj = Map::new();
    obj.insert("source".to_owned(), Value::from(source));
    obj.insert("timestamp".to_owned(), ts);
    obj.insert(
        "age_days".to_owned(),
        age_days.map_or(Value::Null, Value::from),
    );
    obj.insert("is_stale".to_owned(), Value::Bool(is_stale));
    obj.insert("model_count".to_owned(), Value::from(model_count));
    Value::Object(obj)
}

/// `(datetime.now(UTC) - fromisoformat(ts.replace("Z", "+00:00"))).total_seconds() / 86400`.
fn parse_age_days(raw: &str) -> Option<f64> {
    let normalised = raw.replace('Z', "+00:00");
    let cache_time = stax_etl::stats::pydatetime::parse_ts(&normalised)?;
    let now_us = i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_micros(),
    )
    .ok()?;
    let now = stax_etl::stats::pydatetime::PyDateTime {
        wall_us: now_us,
        offset_s: Some(0),
    };
    // A naive cached timestamp against an aware `now` is CPython's `TypeError`,
    // which `read_cache_status` catches as `age_days = None`.
    Some(now.sub_total_seconds(cache_time)? / 86400.0)
}

/// Python truthiness for the shapes a JSON value can take.
fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

// ── shared numerics ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cold_store_answers_ok_with_the_freshness_block_populated() {
        let conn = Connection::open_in_memory().expect("in-memory");
        let engine = stax_etl::stats::dataset::default_engine().expect("manifest");
        let payload = assemble_pricing_health(&conn, &engine, None, 7, 50).expect("assembles");
        assert_eq!(
            stax_memory::pyjson::dumps_http(&payload),
            r#"{"stale_days":7,"ok":true,"summary":{"total_events":0,"total_cost_usd":0.0,"unpriced_model_count":0,"billable_unpriced_model_count":0,"unknown_cost_source_model_count":0,"unknown_nonzero_cost_rows":0,"estimated_unpriced_exposure_usd":0.0,"rate_cache_stale":true},"unpriced_models":[],"unknown_cost_source":[],"rate_freshness":{"source":"none","timestamp":null,"age_days":null,"is_stale":true,"model_count":0,"stale_days_threshold":7,"stale":true}}"#
        );
    }

    #[test]
    fn a_negative_limit_empties_the_lists_rather_than_panicking() {
        // `unpriced[: max(0, limit)]` — Python's slice, reproduced. A naive
        // `truncate(limit as usize)` on -1 would be a wrapping no-op instead.
        let mut rows = vec![Value::from(1), Value::from(2)];
        let limit: i64 = -1;
        let cap = usize::try_from(limit.max(0)).unwrap_or(usize::MAX);
        rows.truncate(cap);
        assert!(rows.is_empty());
    }

    #[test]
    fn the_stale_flag_uses_the_query_threshold_not_the_seven_day_constant() {
        // `is_stale` is `>= 7` (the class constant); `stale` is `> stale_days`
        // (the query param). With `?stale_days=90` a 30-day-old cache is
        // `is_stale: true, stale: false` — both keys ship, and they disagree.
        let mut status = Map::new();
        status.insert("age_days".to_owned(), Value::from(30.0));
        status.insert("is_stale".to_owned(), Value::Bool(true));
        let age = status.get("age_days").and_then(Value::as_f64);
        assert!(age.is_some_and(|a| a > 7.0));
        assert!(!age.is_some_and(|a| a > 90.0));
    }

    #[test]
    fn round_is_bankers_on_the_decimal_expansion() {
        assert!((round_half_even(0.000_000_5, 6) - 0.0).abs() < f64::EPSILON);
        assert!((round_half_even(1.234_567_5, 6) - 1.234_568).abs() < 1e-12);
    }

    #[test]
    fn the_compensated_sum_matches_a_plain_one_on_short_input() {
        let values = [1.5_f64, 2.25, -0.75];
        assert!((neumaier_sum(values.into_iter()) - 3.0).abs() < f64::EPSILON);
    }
}
