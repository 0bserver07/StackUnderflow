//! `routes/whatif.py` — 1 endpoint, wave 5 (batch D).
//!
//! | Item | Method | FastAPI path | axum path | State |
//! |---|---|---|---|---|
//! | `RS-5-113` | `GET` | `/api/whatif` | `/api/whatif` | ported |
//!
//! # What this endpoint actually is
//!
//! A rate-card swap. It reads the four token totals a scope really consumed out
//! of `usage_events`, then reprices *that same token shape* against every entry
//! in `infra/model_candidates.json` through `compute_cost` — used strictly as a
//! black box, exactly as `services/whatif.py` says. Nothing here knows a rate.
//!
//! # The pricing seam is the whole risk (DIV-056)
//!
//! `services/whatif.py` calls `infra.costs.compute_cost`, the module-level
//! function — and a *running server* has already flipped that function's price
//! source to the store's `price_book` table in `_lifespan`
//! (`use_price_book_store` + `prime_price_book_cache`). So the port prices
//! through [`crate::pricing::engine`], which reads the same table, and never
//! through a manifest-only engine. A manifest-only port goes **green on an
//! unprimed store** — the harness home has a populated `price_book`, so the
//! mistake would be invisible in exactly the situation you would test it in and
//! silently mispriced everywhere else. Ten candidates against the whole store's
//! token volume is a large enough lever that a 2% rate difference shows up in
//! the first byte that differs.
//!
//! # Two Python details that are not typos
//!
//! * **`delta_pct` is `None`, not `0.0`, when there is no actual spend.** The
//!   guard is `actual_cost_usd > 0`, so a zero-cost scope emits `null` for every
//!   candidate and the UI renders "n/a" rather than "0%".
//! * **The `model <> ''` filter is spliced two different ways** — `AND` when a
//!   project scope already wrote a `WHERE`, `WHERE` when it did not. Both
//!   branches are reproduced rather than normalised into one, because the
//!   `WHERE`-less branch is the whole-store query and its plan is the one that
//!   matters.
//!
//! FX conversion is a no-op at rate 1.0 (`_convert_payload` returns the payload
//! untouched before reading a field), which is the only rate
//! [`crate::currency`] resolves — DIV-052.

use std::path::Path as StdPath;

use axum::Router;
use axum::extract::{RawQuery, State};
use axum::http::StatusCode;
use axum::routing::get;
use rusqlite::Connection;
use serde_json::{Map, Value};
use stax_etl::pricing::RawTokens;
use stax_etl::pricing::costs::PricingEngine;

use crate::currency::active_currency_payload;
use crate::json::{HandlerResult, HttpError, JsonBody, join_failure};
use crate::pyops::path_name;
use crate::qs::Query;
use crate::services::mart_queries::table_exists;
use crate::state::AppState;

/// Mount this module's endpoints onto `router`.
pub fn register(router: Router<AppState>) -> Router<AppState> {
    router.route("/api/whatif", get(get_whatif))
}

/// One row of `infra/model_candidates.json` — `(pricer, model, label)`.
///
/// `whatif_candidates()` takes **every** entry, `routing_candidate` and all;
/// only `reports/prescribe.py` filters on that flag. Reproduced as written.
#[derive(Debug, Clone)]
struct Candidate {
    pricer: String,
    model: String,
    label: String,
}

/// `TokenTotals` — the 4-way aggregate a repricing operates on.
#[derive(Debug, Clone, Copy, Default)]
struct TokenTotals {
    input: i64,
    output: i64,
    cache_read: i64,
    cache_create: i64,
}

impl TokenTotals {
    /// `TokenTotals.total`.
    const fn total(self) -> i64 {
        self.input + self.output + self.cache_read + self.cache_create
    }

    /// `as_cost_tokens` — note the key is `cache_creation`, not `cache_create`.
    fn as_cost_tokens(self) -> RawTokens {
        RawTokens::canonical(self.input, self.output, self.cache_create, self.cache_read)
    }
}

// ── GET /api/whatif ──────────────────────────────────────────────────────────

async fn get_whatif(State(state): State<AppState>, RawQuery(raw): RawQuery) -> HandlerResult {
    let query = Query::parse(raw.as_deref().unwrap_or_default());
    // `log_path: str | None = None`, with no `Query(...)` wrapper: an absent
    // param is `None` and `?log_path=` is `""`. Both are falsy, and both fall
    // through to `deps.current_log_path`.
    let from_query = query.get("log_path").unwrap_or_default().to_owned();
    let path = if from_query.is_empty() {
        state.current_project().log_path.unwrap_or_default()
    } else {
        from_query
    };

    // Python reads the currency payload BEFORE opening the connection. The order
    // is only observable when it raises, which is exactly the DIV-052 leg.
    let currency = active_currency_payload(&state.config().currency)
        .map_err(|err| HttpError::new(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?;

    let worker = state.clone();
    let worker_path = path.clone();
    let (totals, actual_cost, models, engine) =
        tokio::task::spawn_blocking(move || -> Result<_, HttpError> {
            let conn = worker.connect().map_err(|err| any_500(&err))?;
            let engine =
                crate::pricing::engine(&conn, worker.package_dir()).map_err(|err| any_500(&err))?;
            let (totals, actual_cost, models) = if worker_path.is_empty() {
                token_totals(&conn, None)?
            } else {
                let ids = project_ids_for(&conn, &worker_path)?;
                token_totals(&conn, Some(&ids))?
            };
            Ok((totals, actual_cost, models, engine))
        })
        .await
        .map_err(|err| join_failure(&err))??;

    let (scope, slug) = if path.is_empty() {
        ("all", Value::Null)
    } else {
        ("project", Value::from(path_name(&path)))
    };

    let candidates = load_candidates(state.package_dir()).map_err(|err| any_500(&err))?;
    let mut payload = build_whatif(&engine, totals, actual_cost, &models, &candidates);
    // `_convert_payload` returns before touching a field at rate 1.0, which is
    // the only rate the port resolves (DIV-052) — nothing to scale.
    payload.insert("scope".to_owned(), Value::from(scope));
    payload.insert("project_slug".to_owned(), slug);
    payload.insert("currency".to_owned(), currency);
    Ok(JsonBody::ok(Value::Object(payload)))
}

/// `build_whatif` — key order `tokens`, `actual`, `candidates`, `cheapest`.
fn build_whatif(
    engine: &PricingEngine,
    totals: TokenTotals,
    actual_cost_usd: f64,
    actual_models: &[String],
    candidates: &[Candidate],
) -> Map<String, Value> {
    let rows = reprice(engine, totals, actual_cost_usd, candidates);

    let mut tokens = Map::new();
    tokens.insert("input".to_owned(), Value::from(totals.input));
    tokens.insert("output".to_owned(), Value::from(totals.output));
    tokens.insert("cache_read".to_owned(), Value::from(totals.cache_read));
    tokens.insert("cache_create".to_owned(), Value::from(totals.cache_create));
    tokens.insert("total".to_owned(), Value::from(totals.total()));

    // `sorted(actual_models or [])`. Python compares strings by code point and
    // UTF-8 byte order agrees with code-point order, so `str::cmp` is the same
    // ordering rather than merely a close one.
    let mut sorted_models: Vec<&String> = actual_models.iter().collect();
    sorted_models.sort();

    let mut actual = Map::new();
    // `float(actual_cost_usd)` — always a float, so `0.0` and never `0`.
    actual.insert("cost_usd".to_owned(), Value::from(actual_cost_usd));
    actual.insert(
        "models".to_owned(),
        Value::Array(
            sorted_models
                .into_iter()
                .map(|model| Value::from(model.clone()))
                .collect(),
        ),
    );

    let cheapest = rows.first().cloned().map_or(Value::Null, Value::Object);

    let mut payload = Map::new();
    payload.insert("tokens".to_owned(), Value::Object(tokens));
    payload.insert("actual".to_owned(), Value::Object(actual));
    payload.insert(
        "candidates".to_owned(),
        Value::Array(rows.into_iter().map(Value::Object).collect()),
    );
    payload.insert("cheapest".to_owned(), cheapest);
    payload
}

/// `reprice` — one row per candidate, cheapest first.
fn reprice(
    engine: &PricingEngine,
    totals: TokenTotals,
    actual_cost_usd: f64,
    candidates: &[Candidate],
) -> Vec<Map<String, Value>> {
    let cost_tokens = totals.as_cost_tokens();
    let mut rows: Vec<(f64, Map<String, Value>)> = Vec::with_capacity(candidates.len());
    for candidate in candidates {
        // `compute_cost(tokens, model, provider=provider)` — `speed="standard"`
        // and `at_ts=None` are both the declared defaults.
        let cost = engine
            .compute_cost(
                &cost_tokens,
                &candidate.model,
                &candidate.pricer,
                "standard",
                None,
            )
            .total_cost;
        let delta = cost - actual_cost_usd;
        let delta_pct = if actual_cost_usd > 0.0 {
            Value::from(delta / actual_cost_usd * 100.0)
        } else {
            // `None`, not `0.0` — see the module docs.
            Value::Null
        };
        let mut row = Map::new();
        row.insert("provider".to_owned(), Value::from(candidate.pricer.clone()));
        row.insert("model".to_owned(), Value::from(candidate.model.clone()));
        row.insert("label".to_owned(), Value::from(candidate.label.clone()));
        row.insert("cost_usd".to_owned(), Value::from(cost));
        row.insert("delta_usd".to_owned(), Value::from(delta));
        row.insert("delta_pct".to_owned(), delta_pct);
        rows.push((cost, row));
    }
    // `rows.sort(key=lambda r: r["cost_usd"])` — Timsort is stable, so equal
    // costs keep catalogue order. `sort_by` is stable too; `sort_unstable_by`
    // would be a silent divergence across the several $0.00 candidates an
    // unpriced token shape produces.
    rows.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    rows.into_iter().map(|(_, row)| row).collect()
}

/// `_token_totals` — the four sums, the actual cost, and the distinct models.
fn token_totals(
    conn: &Connection,
    project_ids: Option<&[i64]>,
) -> Result<(TokenTotals, f64, Vec<String>), HttpError> {
    if !table_exists(conn, "usage_events").map_err(sql_500)? {
        return Ok((TokenTotals::default(), 0.0, Vec::new()));
    }
    let mut where_clause = String::new();
    if let Some(ids) = project_ids {
        if ids.is_empty() {
            return Ok((TokenTotals::default(), 0.0, Vec::new()));
        }
        let placeholders = vec!["?"; ids.len()].join(",");
        where_clause = format!("WHERE project_id IN ({placeholders})");
    }
    let params: Vec<i64> = project_ids.unwrap_or(&[]).to_vec();
    let bound: Vec<&dyn rusqlite::ToSql> =
        params.iter().map(|id| id as &dyn rusqlite::ToSql).collect();

    let sql = format!(
        "SELECT \
           COALESCE(SUM(input_tokens), 0)        AS it, \
           COALESCE(SUM(output_tokens), 0)       AS ot, \
           COALESCE(SUM(cache_read_tokens), 0)   AS crt, \
           COALESCE(SUM(cache_create_tokens), 0) AS cct, \
           COALESCE(SUM(cost_usd), 0.0)          AS cost \
         FROM usage_events {where_clause}"
    );
    let (totals, actual_cost) = conn
        .query_row(&sql, bound.as_slice(), |row| {
            Ok((
                TokenTotals {
                    input: row.get::<_, Option<i64>>(0)?.unwrap_or(0),
                    output: row.get::<_, Option<i64>>(1)?.unwrap_or(0),
                    cache_read: row.get::<_, Option<i64>>(2)?.unwrap_or(0),
                    cache_create: row.get::<_, Option<i64>>(3)?.unwrap_or(0),
                },
                row.get::<_, Option<f64>>(4)?.unwrap_or(0.0),
            ))
        })
        .map_err(sql_500)?;

    let mut model_sql = format!("SELECT DISTINCT model FROM usage_events {where_clause}");
    if where_clause.is_empty() {
        model_sql.push_str(" WHERE model <> ''");
    } else {
        model_sql.push_str(" AND model <> ''");
    }
    let mut stmt = conn.prepare(&model_sql).map_err(sql_500)?;
    let models: Vec<String> = stmt
        .query_map(bound.as_slice(), |row| row.get::<_, Option<String>>(0))
        .map_err(sql_500)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(sql_500)?
        .into_iter()
        // `if r[0]` — a NULL or empty model is dropped a second time in Python,
        // after the SQL already excluded `''`.
        .flatten()
        .filter(|model| !model.is_empty())
        .collect();

    Ok((totals, actual_cost, models))
}

/// `_project_ids_for` — `queries.get_projects_by_slug`, then the 404.
///
/// The column list is the query helper's, not `SELECT id`: the reference
/// materialises a whole `ProjectRow` and only reads `.id`, and the wider row is
/// what its plan touches.
fn project_ids_for(conn: &Connection, path: &str) -> Result<Vec<i64>, HttpError> {
    let slug = path_name(path);
    let mut stmt = conn
        .prepare(
            "SELECT id, provider, slug, path, display_name, first_seen, last_modified \
             FROM projects WHERE slug = ?",
        )
        .map_err(sql_500)?;
    let ids: Vec<i64> = stmt
        .query_map([&slug], |row| row.get(0))
        .map_err(sql_500)?
        .collect::<rusqlite::Result<_>>()
        .map_err(sql_500)?;
    if ids.is_empty() {
        return Err(HttpError::not_found(format!(
            "Project '{slug}' not found in store — try /api/refresh first"
        )));
    }
    Ok(ids)
}

/// `infra/model_catalog._load` — every `candidates` entry, in file order.
///
/// Read from the injected package directory rather than embedded, for the same
/// reason `pricing::manifest_path` is: the harness points both servers at the
/// *same* `stackunderflow/` tree, so a catalogue edit lands on both sides at
/// once and cannot skew.
fn load_candidates(package_dir: &StdPath) -> anyhow::Result<Vec<Candidate>> {
    let path = package_dir.join("infra").join("model_candidates.json");
    let text = std::fs::read_to_string(&path)
        .map_err(|err| anyhow::anyhow!("reading {}: {err}", path.display()))?;
    let parsed: Value = serde_json::from_str(&text)?;
    let entries = parsed
        .get("candidates")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("{}: no `candidates` array", path.display()))?;
    Ok(entries
        .iter()
        .map(|entry| Candidate {
            pricer: string_at(entry, "pricer"),
            model: string_at(entry, "model"),
            label: string_at(entry, "label"),
        })
        .collect())
}

fn string_at(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
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

    fn package_dir() -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../stackunderflow")
    }

    fn engine() -> PricingEngine {
        PricingEngine::from_manifest_path(&crate::pricing::manifest_path(&package_dir()))
            .expect("the checked-in rate card loads")
    }

    fn candidates() -> Vec<Candidate> {
        load_candidates(&package_dir()).expect("the checked-in catalogue loads")
    }

    #[test]
    fn totals_sum_all_four_buckets() {
        let totals = TokenTotals {
            input: 10,
            output: 20,
            cache_read: 30,
            cache_create: 40,
        };
        assert_eq!(totals.total(), 100);
    }

    #[test]
    fn the_cost_token_shape_swaps_the_last_two_positions() {
        // `as_cost_tokens` emits `cache_creation` third and `cache_read` fourth
        // while `TokenTotals` declares them the other way round. Getting it
        // backwards prices cache writes at read rates and still returns a
        // perfectly plausible number.
        let totals = TokenTotals {
            input: 1,
            output: 2,
            cache_read: 3,
            cache_create: 4,
        };
        assert_eq!(
            format!("{:?}", totals.as_cost_tokens()),
            format!("{:?}", RawTokens::canonical(1, 2, 4, 3))
        );
    }

    #[test]
    fn the_catalogue_is_every_entry_not_the_routing_subset() {
        // `whatif_candidates()` ignores `routing_candidate`; `gpt-5-codex` is
        // flagged `false` and must still be in the comparison set.
        let rows = candidates();
        assert!(rows.iter().any(|c| c.model == "gpt-5-codex"));
        assert!(rows.len() >= 10, "catalogue shrank to {}", rows.len());
    }

    #[test]
    fn no_actual_spend_makes_every_delta_pct_null() {
        let rows = reprice(
            &engine(),
            TokenTotals {
                input: 1_000,
                output: 1_000,
                cache_read: 0,
                cache_create: 0,
            },
            0.0,
            &candidates(),
        );
        assert!(!rows.is_empty());
        for row in &rows {
            assert_eq!(row["delta_pct"], Value::Null, "{:?}", row["model"]);
        }
    }

    #[test]
    fn candidates_come_back_cheapest_first() {
        let rows = reprice(
            &engine(),
            TokenTotals {
                input: 1_000_000,
                output: 100_000,
                cache_read: 0,
                cache_create: 0,
            },
            1.0,
            &candidates(),
        );
        let costs: Vec<f64> = rows
            .iter()
            .map(|row| row["cost_usd"].as_f64().expect("float"))
            .collect();
        assert!(
            costs.windows(2).all(|pair| pair[0] <= pair[1]),
            "not ascending: {costs:?}"
        );
        // `delta_usd = cost - actual`, so the cheapest row carries the most
        // negative delta.
        assert!(
            (rows[0]["delta_usd"].as_f64().expect("float") - (costs[0] - 1.0)).abs() < f64::EPSILON
        );
    }

    #[test]
    fn the_payload_key_order_is_the_literals() {
        let payload = build_whatif(
            &engine(),
            TokenTotals::default(),
            0.0,
            &["b".to_owned(), "a".to_owned()],
            &[],
        );
        let keys: Vec<&str> = payload.keys().map(String::as_str).collect();
        assert_eq!(keys, ["tokens", "actual", "candidates", "cheapest"]);
        // An empty candidate set → `cheapest` is `null`, not `{}`.
        assert_eq!(payload["cheapest"], Value::Null);
        // `sorted(actual_models or [])`.
        assert_eq!(payload["actual"]["models"], serde_json::json!(["a", "b"]));
        // `float(actual_cost_usd)` renders `0.0`, never `0`.
        assert!(
            JsonBody::ok(Value::Object(payload))
                .render()
                .contains(r#""cost_usd":0.0"#)
        );
    }
}
