//! `routes/benchmark.py` — 2 endpoints, wave 5 (batch E). Was DIV-143.
//!
//! | Item | Method | FastAPI path | axum path | State |
//! |---|---|---|---|---|
//! | `RS-5-045` | `GET` | `/api/benchmark          ` | `/api/benchmark`           | **ported** |
//! | `RS-5-046` | `GET` | `/api/benchmark/recommend` | `/api/benchmark/recommend` | **ported** |
//!
//! Two thin shells over [`crate::services::benchmark`], which is where the
//! 1 033-line report and its statistics live. This module does what Python's
//! does: alias the period, resolve the project, call one function, stamp
//! currency, and return.
//!
//! # The four things the route layer itself decides
//!
//! 1. **The period alias table is this module's, not `scope.py`'s.**
//!    `_PERIOD_ALIASES` maps a friendly superset (`week`, `30days`, …) onto
//!    `parse_period`'s specs, and the `400` message is
//!    `', '.join(_PERIOD_ALIASES)` — a join over the **dict keys in insertion
//!    order**, so it reads `today, week, 7days, month, 30days, all` and NOT the
//!    sorted list `routes/optimize.py` prints. Two endpoints, two orders, both
//!    ported as written.
//! 2. **`week` is `7days`, which is a rolling `now - 7d` instant.** Batch A
//!    measured that on `_by_model_mart_eligible`: the bound carries the current
//!    microsecond, so the two servers in the differ compute bounds a few
//!    milliseconds apart and a session whose `first_ts` lands in that gap is a
//!    real divergence. The harness store's last session starts
//!    2026-07-30T16:32, six days clear of the boundary, so the row is safe
//!    today and stays inherently time-sensitive. Same property as
//!    `CD-prov-week`.
//! 3. **An unresolvable `log_path` is an EMPTY report, not a 404.**
//!    `_project_ids_for` is this module's own resolver: it returns `[]` for an
//!    unknown slug and swallows a bad store, and `_load_facts` reads
//!    `project_ids == []` as "no sessions". `routes/cost.py`'s resolver raises a
//!    404 on the same input. The asymmetry is real and both are ported where
//!    they live.
//! 4. **Currency conversion is an explicit walk, outside the report.** Python
//!    deep-copies the cached report and multiplies exactly four places:
//!    `verdict.cost_per_outcome_usd`, and each model row's `cost_per_outcome`
//!    and `median_cost` blocks (`point` plus a two-element `ci`). `ci_wilson` is
//!    a proportion and is deliberately NOT scaled. The walk is ported; it is
//!    unreachable behind DIV-052 (only USD resolves), so no case row can
//!    measure it and it is covered by unit test instead.
//!
//! # The read-through cache is NOT ported
//!
//! `_BENCH_CACHE` is a process-wide dict keyed on `(store, scope, ids, intent)`
//! and validated by a `(MAX(last_ts), SUM(message_count))` signature, holding
//! the **USD** report; currency is applied to a `copy.deepcopy` outside it. It
//! is a pure memo — the entry it returns is byte-identical to a recompute
//! against the same store revision, and it publishes nothing about itself (no
//! `"cache": "hit"` field). That is DIV-055/DIV-091's disposition, not
//! DIV-111's: `/api/optimize` had to port its cache because the cache state was
//! *in the body*. This one is not.
//!
//! What it costs is latency, not bytes: `PL-plan-repeat`'s trick applies here
//! too, and `BM-benchmark-repeat` in the case file is the row that proves a
//! second identical request returns the same bytes on both sides.
//!
//! Read-only. Every row in the case file is a `GET` or an unclaimed method.

use axum::Router;
use axum::extract::{RawQuery, State};
use axum::http::StatusCode;
use axum::routing::get;
use rusqlite::Connection;
use serde_json::{Map, Value};

use crate::currency::active_currency_payload;
use crate::json::{HandlerResult, HttpError, JsonBody, join_failure, missing_query_param};
use crate::pyops::path_name;
use crate::qs::Query;
use crate::services::benchmark::{Weights, analyze_benchmark, recommend_from_history};
use crate::services::benchmark_stats as bs;
use crate::services::scope::{Instant, Scope, parse_period};
use crate::state::AppState;

/// `_PERIOD_ALIASES` — **insertion order**, because the `400` joins the keys.
const PERIOD_ALIASES: [(&str, &str); 6] = [
    ("today", "today"),
    ("week", "7days"),
    ("7days", "7days"),
    ("month", "month"),
    ("30days", "30days"),
    ("all", "all"),
];

/// Mount this module's endpoints onto `router`.
pub fn register(router: Router<AppState>) -> Router<AppState> {
    router
        .route("/api/benchmark", get(get_benchmark))
        .route("/api/benchmark/recommend", get(get_benchmark_recommend))
}

// ── shared helpers ───────────────────────────────────────────────────────────

/// `_PERIOD_ALIASES.get(period)`, or the `400` Python raises.
///
/// `period = period if isinstance(period, str) else "all"` cannot fire from a
/// query string — everything arriving here is already a `str` — so the coercion
/// guard is a no-op and the lookup is the whole validation.
fn period_scope(period: &str) -> Result<Scope, HttpError> {
    let Some((_, spec)) = PERIOD_ALIASES.iter().find(|(alias, _)| *alias == period) else {
        let valid = PERIOD_ALIASES
            .iter()
            .map(|(alias, _)| *alias)
            .collect::<Vec<_>>()
            .join(", ");
        return Err(HttpError::bad_request(format!(
            "Invalid period '{period}'. Valid: {valid}"
        )));
    };
    // Every spec in the table is one `parse_period` knows, so the `Err` arm is
    // structurally unreachable; it is mapped rather than unwrapped so a future
    // alias cannot panic a handler.
    parse_period(spec, Instant::now_utc()).map_err(HttpError::bad_request)
}

/// `log_path or deps.current_log_path`, then `if path`.
///
/// Both `or` and `if` are Python truthiness, so an EMPTY `?log_path=` falls back
/// to the selected project and an empty selection means "whole store".
fn resolve_path(query: &Query, state: &AppState) -> Option<String> {
    query
        .get("log_path")
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .or_else(|| {
            state
                .current_project()
                .log_path
                .filter(|value| !value.is_empty())
        })
}

/// `_project_ids_for(conn, path)` — this module's own resolver.
///
/// No 404: an unknown slug is `[]`, which `_load_facts` turns into an empty
/// report. A store error is `[]` too (`except Exception: return []`).
fn project_ids_for(conn: &Connection, path: &str) -> Vec<i64> {
    let slug = path_name(path);
    let Ok(mut stmt) = conn.prepare("SELECT id FROM projects WHERE slug = ?") else {
        return Vec::new();
    };
    stmt.query_map([&slug], |row| row.get::<_, i64>(0))
        .map(|rows| rows.flatten().collect())
        .unwrap_or_default()
}

/// `_convert_cost_block(block, rate)` — scale `{"point", "ci"}` in place.
///
/// `if not isinstance(block, dict): return` — a `None` block (the shape
/// `cost_per_outcome` takes when there are no successes) is skipped whole.
/// `block["ci"]` is only rewritten when it is a list, so a `null` CI stays null.
fn convert_cost_block(block: Option<&mut Value>, rate: f64) {
    let Some(Value::Object(block)) = block else {
        return;
    };
    if let Some(point) = block.get("point").and_then(Value::as_f64) {
        block.insert("point".to_owned(), Value::from(point * rate));
    }
    let scaled = block.get("ci").and_then(Value::as_array).map(|ci| {
        Value::Array(
            ci.iter()
                .map(|x| Value::from(x.as_f64().unwrap_or(0.0) * rate))
                .collect(),
        )
    });
    if let Some(scaled) = scaled {
        block.insert("ci".to_owned(), scaled);
    }
}

/// `_convert_report_costs(report, rate)` — the explicit four-place walk.
///
/// Never a blanket multiply: `success_rate` is a proportion and `median_turns` a
/// count, and both sit in the same rows. `cost_usd` is only *displayed* in
/// another currency; the mart invariant is untouched.
fn convert_report_costs(report: &mut Value, rate: f64) {
    if rate == 1.0 {
        return;
    }
    if let Some(Value::Object(verdict)) = report.get_mut("verdict")
        && let Some(usd) = verdict.get("cost_per_outcome_usd").and_then(Value::as_f64)
    {
        verdict.insert("cost_per_outcome_usd".to_owned(), Value::from(usd * rate));
    }
    let Some(Value::Array(strata)) = report.get_mut("strata") else {
        return;
    };
    for stratum in strata.iter_mut() {
        let Some(Value::Array(models)) = stratum.get_mut("models") else {
            continue;
        };
        for model in models.iter_mut() {
            convert_cost_block(model.get_mut("cost_per_outcome"), rate);
            convert_cost_block(model.get_mut("median_cost"), rate);
        }
    }
}

fn any_500(err: &anyhow::Error) -> HttpError {
    HttpError::new(StatusCode::INTERNAL_SERVER_ERROR, err.to_string())
}

/// `currency["rate_from_usd"]`.
fn rate_from(currency: &Value) -> f64 {
    currency
        .get("rate_from_usd")
        .and_then(Value::as_f64)
        .unwrap_or(1.0)
}

// ── GET /api/benchmark ───────────────────────────────────────────────────────

async fn get_benchmark(State(state): State<AppState>, RawQuery(raw): RawQuery) -> HandlerResult {
    let query = Query::parse(raw.as_deref().unwrap_or_default());
    let period = query.str_or("period", "all").to_owned();
    let scope = period_scope(&period)?;
    let intent = query.get("intent").map(str::to_owned);
    let path = resolve_path(&query, &state);

    let worker = state.clone();
    let scope_for_worker = scope.clone();
    let mut report = tokio::task::spawn_blocking(move || -> Result<Value, HttpError> {
        let conn = worker.connect().map_err(|err| any_500(&err))?;
        let project_ids = path.map(|path| project_ids_for(&conn, &path));
        Ok(analyze_benchmark(
            &conn,
            Some(&scope_for_worker),
            project_ids.as_deref(),
            intent.as_deref(),
            Weights::default(),
            bs::CI_LEVEL,
        ))
    })
    .await
    .map_err(|err| join_failure(&err))??;

    let currency = active_currency_payload(&state.config().currency)
        .map_err(|err| HttpError::new(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?;
    convert_report_costs(&mut report, rate_from(&currency));

    // `report.get("warning")` is read AFTER the conversion, from the same
    // object that goes out under `"report"` — one value, serialised twice.
    let warning = report.get("warning").cloned().unwrap_or(Value::Null);

    let mut payload = Map::new();
    payload.insert("period".to_owned(), Value::from(period));
    payload.insert("scope".to_owned(), Value::from(scope.label));
    payload.insert("report".to_owned(), report);
    payload.insert("currency".to_owned(), currency);
    payload.insert("warning".to_owned(), warning);
    Ok(JsonBody::ok(Value::Object(payload)))
}

// ── GET /api/benchmark/recommend ─────────────────────────────────────────────

async fn get_benchmark_recommend(
    State(state): State<AppState>,
    RawQuery(raw): RawQuery,
) -> HandlerResult {
    let query = Query::parse(raw.as_deref().unwrap_or_default());

    // `intent: str = Query(...)` is REQUIRED, and FastAPI's dependency solver
    // rejects an absent one before the handler body runs — so this 422 comes
    // FIRST, ahead of the period check below, even for `?period=nonsense`.
    let Some(intent) = query.get("intent").map(str::to_owned) else {
        return Ok(JsonBody::with_status(
            StatusCode::UNPROCESSABLE_ENTITY,
            missing_query_param("intent"),
        ));
    };

    let period = query.str_or("period", "all").to_owned();
    let scope = period_scope(&period)?;
    // …and the emptiness check comes AFTER the period one, inside the body.
    if intent.trim().is_empty() {
        return Err(HttpError::bad_request("intent is required"));
    }

    let size = query.get("size").map(str::to_owned);
    let language = query.get("language").map(str::to_owned);
    let path = resolve_path(&query, &state);

    let worker = state.clone();
    let scope_for_worker = scope.clone();
    let mut rec = tokio::task::spawn_blocking(move || -> Result<Value, HttpError> {
        let conn = worker.connect().map_err(|err| any_500(&err))?;
        let project_ids = path.map(|path| project_ids_for(&conn, &path));
        Ok(recommend_from_history(
            &conn,
            &intent,
            size.as_deref(),
            language.as_deref(),
            Some(&scope_for_worker),
            project_ids.as_deref(),
            Weights::default(),
            bs::CI_LEVEL,
        ))
    })
    .await
    .map_err(|err| join_failure(&err))??;

    let currency = active_currency_payload(&state.config().currency)
        .map_err(|err| HttpError::new(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?;
    let rate = rate_from(&currency);
    // `if rate != 1.0 and isinstance(rec.get("evidence"), dict)` — the guard is
    // on the whole pair, so a null evidence block is skipped entirely.
    if rate != 1.0
        && let Some(evidence @ Value::Object(_)) = rec.get_mut("evidence")
    {
        convert_cost_block(evidence.get_mut("cost_per_outcome"), rate);
        convert_cost_block(evidence.get_mut("median_cost"), rate);
    }

    let mut payload = Map::new();
    payload.insert("period".to_owned(), Value::from(period));
    payload.insert("scope".to_owned(), Value::from(scope.label));
    payload.insert("recommendation".to_owned(), rec);
    payload.insert("currency".to_owned(), currency);
    Ok(JsonBody::ok(Value::Object(payload)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_invalid_period_message_joins_the_keys_in_insertion_order() {
        // NOT the sorted order `/api/optimize` prints — this table is a dict
        // and `', '.join(dict)` walks it as inserted.
        let err = period_scope("nonsense").expect_err("unknown alias");
        assert_eq!(
            err.body().render(),
            r#"{"detail":"Invalid period 'nonsense'. Valid: today, week, 7days, month, 30days, all"}"#
        );
        // An empty `?period=` is an unknown alias too, quoted as the empty
        // string — the default only applies when the key is ABSENT.
        let err = period_scope("").expect_err("the empty string is not an alias");
        assert_eq!(
            err.body().render(),
            r#"{"detail":"Invalid period ''. Valid: today, week, 7days, month, 30days, all"}"#
        );
    }

    #[test]
    fn every_alias_resolves_and_week_is_the_rolling_seven_days() {
        for (alias, expected_label) in [
            ("today", "today"),
            ("week", "last 7 days"),
            ("7days", "last 7 days"),
            ("30days", "last 30 days"),
            ("all", "all time"),
        ] {
            let scope = period_scope(alias).expect("a known alias");
            assert_eq!(scope.label, expected_label, "period={alias}");
        }
        // `month`'s label carries the calendar month, so it is asserted loosely.
        assert!(
            period_scope("month")
                .expect("a known alias")
                .label
                .starts_with("this month (")
        );
        // `all` is the only unbounded one — `week` carries live bounds.
        assert_eq!(period_scope("all").expect("known").since, None);
        assert!(period_scope("week").expect("known").since.is_some());
    }

    #[test]
    fn the_currency_walk_scales_costs_and_leaves_proportions_alone() {
        // The four places, and the two that must NOT move.
        let mut report = serde_json::json!({
            "verdict": {"cost_per_outcome_usd": 2.0, "confidence": "high"},
            "strata": [{
                "models": [{
                    "success_rate": {"point": 0.5, "ci_wilson": [0.25, 0.75]},
                    "cost_per_outcome": {"point": 4.0, "ci": [2.0, 8.0]},
                    "median_cost": {"point": 1.0, "ci": [0.5, 1.5]},
                    "median_turns": 10,
                    "reasoning_share": 0.5
                }, {
                    // The live shape: no successes, so the whole block is null.
                    "success_rate": {"point": 0.0, "ci_wilson": null},
                    "cost_per_outcome": {"point": null, "ci": null},
                    "median_cost": {"point": 3.0, "ci": [3.0, 3.0]},
                    "median_turns": 7,
                    "reasoning_share": 0.0
                }]
            }]
        });
        convert_report_costs(&mut report, 2.0);
        assert_eq!(report["verdict"]["cost_per_outcome_usd"], 4.0);
        let rows = &report["strata"][0]["models"];
        assert_eq!(rows[0]["cost_per_outcome"]["point"], 8.0);
        assert_eq!(
            rows[0]["cost_per_outcome"]["ci"],
            serde_json::json!([4.0, 16.0])
        );
        assert_eq!(rows[0]["median_cost"]["ci"], serde_json::json!([1.0, 3.0]));
        // A proportion, a count and a share are all untouched.
        assert_eq!(rows[0]["success_rate"]["point"], 0.5);
        assert_eq!(
            rows[0]["success_rate"]["ci_wilson"],
            serde_json::json!([0.25, 0.75])
        );
        assert_eq!(rows[0]["median_turns"], 10);
        assert_eq!(rows[0]["reasoning_share"], 0.5);
        // A null point and a null ci survive as nulls.
        assert_eq!(rows[1]["cost_per_outcome"]["point"], Value::Null);
        assert_eq!(rows[1]["cost_per_outcome"]["ci"], Value::Null);
        assert_eq!(rows[1]["median_cost"]["point"], 6.0);
    }

    #[test]
    fn a_rate_of_one_is_a_no_op_including_on_the_bytes() {
        let original = serde_json::json!({
            "verdict": {"cost_per_outcome_usd": 2.0},
            "strata": [{"models": [{"median_cost": {"point": 1.0, "ci": [0.5, 1.5]}}]}]
        });
        let mut report = original.clone();
        convert_report_costs(&mut report, 1.0);
        assert_eq!(report, original);
        // …and the USD payload the harness serves has exactly that rate.
        let currency = active_currency_payload("USD").expect("USD resolves");
        assert_eq!(rate_from(&currency), 1.0);
        assert_eq!(
            JsonBody::ok(currency).render(),
            r#"{"code":"USD","symbol":"$","rate_from_usd":1.0,"warning":null}"#
        );
    }

    #[test]
    fn the_verdict_conversion_skips_a_null_cost_per_outcome() {
        // The live verdict: `cost_per_outcome_usd` is null, so the `is not None`
        // guard leaves it null rather than multiplying it into a 0.
        let mut report = serde_json::json!({
            "verdict": {"cost_per_outcome_usd": Value::Null},
            "strata": []
        });
        convert_report_costs(&mut report, 3.0);
        assert_eq!(report["verdict"]["cost_per_outcome_usd"], Value::Null);
    }

    #[test]
    fn the_missing_intent_422_is_pydantics_shape_not_a_400() {
        // `intent: str = Query(...)` — an ABSENT parameter never reaches the
        // handler, so it is `missing`, not `"intent is required"`.
        assert_eq!(
            JsonBody::with_status(
                StatusCode::UNPROCESSABLE_ENTITY,
                missing_query_param("intent")
            )
            .render(),
            r#"{"detail":[{"type":"missing","loc":["query","intent"],"msg":"Field required","input":null}]}"#
        );
        // A PRESENT but blank one does reach it, and is the 400.
        assert_eq!(
            HttpError::bad_request("intent is required").body().render(),
            r#"{"detail":"intent is required"}"#
        );
    }

    // ── in-process, over the engine's synthetic fixture ──────────────────────
    //
    // `oneshot` drives the mounted router with no port, so nothing here can
    // collide with the reserved :8095 / :8096. The store is the one
    // `services::benchmark`'s fixture builds, whose report is already pinned to
    // CPython's bytes there — these rows pin the ENVELOPE around it.

    /// A scratch `STACKUNDERFLOW_HOME` that cleans itself up.
    struct Scratch(std::path::PathBuf);

    impl Scratch {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "stax-benchmark-{tag}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |delta| delta.as_nanos())
            ));
            std::fs::create_dir_all(&dir).expect("mkdir");
            Self(dir)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).ok();
        }
    }

    fn seeded_state(scratch: &Scratch) -> AppState {
        let store = scratch.0.join("store.db");
        let conn = Connection::open(&store).expect("open");
        conn.execute_batch(crate::services::benchmark::FIXTURE_SQL)
            .expect("seed");
        drop(conn);
        AppState::new(store, scratch.0.clone(), crate::state::Config::default())
    }

    async fn get(state: &AppState, target: &str) -> (StatusCode, Option<String>, String) {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt as _;

        let app = register(axum::Router::new()).with_state(state.clone());
        let response = app
            .oneshot(
                Request::builder()
                    .uri(target)
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        let status = response.status();
        let content_type = response
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body");
        (
            status,
            content_type,
            String::from_utf8(bytes.to_vec()).expect("utf-8"),
        )
    }

    #[tokio::test]
    async fn the_happy_path_wraps_the_report_in_period_scope_currency_and_warning() {
        let scratch = Scratch::new("ok");
        let state = seeded_state(&scratch);
        let (status, content_type, body) = get(&state, "/api/benchmark").await;
        assert_eq!(status, StatusCode::OK);
        // starlette appends `; charset=utf-8` only to `text/*`.
        assert_eq!(content_type.as_deref(), Some("application/json"));

        let expected = format!(
            concat!(
                r#"{{"period":"all","scope":"all time","report":{},"#,
                r#""currency":{{"code":"USD","symbol":"$","rate_from_usd":1.0,"warning":null}},"#,
                r#""warning":"{}"}}"#
            ),
            crate::services::benchmark::FIXTURE_REPORT,
            crate::services::benchmark::NATURAL_EXPERIMENT_WARNING,
        );
        assert_eq!(body, expected);
        // The top-level `warning` is `report["warning"]`, read back out of the
        // same object — not a second copy that could drift.
        assert!(body.contains("you already ran — a natural experiment"));
    }

    #[tokio::test]
    async fn every_valid_period_answers_200_and_echoes_its_own_spelling() {
        let scratch = Scratch::new("periods");
        let state = seeded_state(&scratch);
        for (period, label) in [
            ("today", "today"),
            ("week", "last 7 days"),
            ("7days", "last 7 days"),
            ("30days", "last 30 days"),
            ("all", "all time"),
        ] {
            let (status, _, body) = get(&state, &format!("/api/benchmark?period={period}")).await;
            assert_eq!(status, StatusCode::OK, "period={period}");
            // `"period"` echoes the ALIAS the caller sent, while `"scope"` is
            // the resolved label — so `week` and `7days` differ in the payload
            // even though they analyse the same window.
            assert!(
                body.starts_with(&format!(r#"{{"period":"{period}","scope":"{label}","#)),
                "period={period}: {}",
                &body[..80.min(body.len())]
            );
        }
    }

    #[tokio::test]
    async fn an_unknown_period_is_the_400_with_the_insertion_ordered_key_list() {
        let scratch = Scratch::new("badperiod");
        let state = seeded_state(&scratch);
        for target in [
            "/api/benchmark?period=nonsense",
            "/api/benchmark/recommend?intent=build&period=nonsense",
        ] {
            let (status, content_type, body) = get(&state, target).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{target}");
            assert_eq!(content_type.as_deref(), Some("application/json"));
            assert_eq!(
                body,
                r#"{"detail":"Invalid period 'nonsense'. Valid: today, week, 7days, month, 30days, all"}"#
            );
        }
    }

    #[tokio::test]
    async fn the_recommend_validation_order_puts_the_missing_intent_first() {
        let scratch = Scratch::new("recvalidation");
        let state = seeded_state(&scratch);

        // Absent `intent` — rejected by the dependency solver, so the 422 wins
        // even when `period` is ALSO invalid.
        for target in [
            "/api/benchmark/recommend",
            "/api/benchmark/recommend?period=nonsense",
        ] {
            let (status, _, body) = get(&state, target).await;
            assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{target}");
            assert_eq!(
                body,
                r#"{"detail":[{"type":"missing","loc":["query","intent"],"msg":"Field required","input":null}]}"#
            );
        }

        // Present but blank — reaches the handler, and the PERIOD check runs
        // before the emptiness check, so a bad period still wins.
        let (status, _, body) = get(&state, "/api/benchmark/recommend?intent=&period=zzz").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body.contains("Invalid period 'zzz'"), "{body}");

        // …and with a good period, the blank intent is the 400.
        for target in [
            "/api/benchmark/recommend?intent=",
            "/api/benchmark/recommend?intent=%20%20",
        ] {
            let (status, _, body) = get(&state, target).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{target}");
            assert_eq!(body, r#"{"detail":"intent is required"}"#);
        }
    }

    #[tokio::test]
    async fn the_recommendation_envelope_is_period_scope_recommendation_currency() {
        let scratch = Scratch::new("rec");
        let state = seeded_state(&scratch);
        let (status, _, body) =
            get(&state, "/api/benchmark/recommend?intent=build&size=tiny").await;
        assert_eq!(status, StatusCode::OK);
        let expected = format!(
            concat!(
                r#"{{"period":"all","scope":"all time","recommendation":{},"#,
                r#""currency":{{"code":"USD","symbol":"$","rate_from_usd":1.0,"warning":null}}}}"#
            ),
            crate::services::benchmark::FIXTURE_RECOMMENDATION,
        );
        assert_eq!(body, expected);
    }

    #[tokio::test]
    async fn an_unknown_log_path_is_an_empty_report_and_not_a_404() {
        let scratch = Scratch::new("unknownslug");
        let state = seeded_state(&scratch);
        let (status, _, body) = get(&state, "/api/benchmark?log_path=/nope/-not-a-project").await;
        // `routes/cost.py` would 404 on this. This module resolves to `[]`,
        // which `_load_facts` reads as "no sessions".
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains(r#""sessions_total":0"#), "{body}");
        assert!(body.contains(r#""strata":[]"#));
        assert!(body.contains(r#""headline":"insufficient evidence""#));

        // An EMPTY `?log_path=` is falsy, so it falls through to the selected
        // project — which is unset here, meaning the whole store.
        let (status, _, body) = get(&state, "/api/benchmark?log_path=").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains(r#""sessions_total":44"#), "{}", &body[..200]);
    }

    #[tokio::test]
    async fn the_selected_project_is_the_default_scope_and_a_query_path_overrides_it() {
        let scratch = Scratch::new("selected");
        let state = seeded_state(&scratch);
        state.set_current_project(crate::state::CurrentProject {
            project_path: None,
            log_path: Some("/logs/-p-bench".to_owned()),
        });
        let (status, _, body) = get(&state, "/api/benchmark").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains(r#""sessions_total":44"#));

        // An explicit unknown path beats the selection.
        let (_, _, body) = get(&state, "/api/benchmark?log_path=/logs/-p-other").await;
        assert!(body.contains(r#""sessions_total":0"#));
    }

    #[tokio::test]
    async fn a_blank_intent_filter_does_not_filter() {
        let scratch = Scratch::new("blankintent");
        let state = seeded_state(&scratch);
        // `if intent:` is truthiness, so `?intent=` is the UNFILTERED report —
        // not an empty one.
        let (status, _, blank) = get(&state, "/api/benchmark?intent=").await;
        assert_eq!(status, StatusCode::OK);
        let (_, _, absent) = get(&state, "/api/benchmark").await;
        assert_eq!(blank, absent);
        // …while a real intent narrows it to one stratum.
        let (_, _, filtered) = get(&state, "/api/benchmark?intent=build").await;
        assert!(
            filtered.contains(r#""sessions_total":22"#),
            "{}",
            &filtered[..200]
        );
        // …and an unknown one empties it without erroring.
        let (status, _, unknown) = get(&state, "/api/benchmark?intent=nosuchintent").await;
        assert_eq!(status, StatusCode::OK);
        assert!(unknown.contains(r#""sessions_total":0"#));
    }

    #[tokio::test]
    async fn repeating_the_request_returns_the_same_bytes() {
        // Python memoises the report and the port does not; the memo cannot
        // change the answer within one store revision, and this is the cheapest
        // available proof of that.
        let scratch = Scratch::new("repeat");
        let state = seeded_state(&scratch);
        let (_, _, first) = get(&state, "/api/benchmark").await;
        let (_, _, second) = get(&state, "/api/benchmark").await;
        assert_eq!(first, second);
    }
}
